#!/usr/bin/env python3
"""Reserve shared filesystem headroom before a managed local build writes."""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import plistlib
import shutil
import stat
import subprocess
import sys
import time
import uuid

GIB = 1024**3
DEFAULT_POLICY = {
    "floor": 24 * GIB,
    "dev": 16 * GIB,
    "release": 24 * GIB,
    "desktop": 24 * GIB,
    "checkout": 2 * GIB,
    "aux_floor": GIB,
    "aux": 2 * GIB,
}
BUILD_ACTIONS = {"build", "b", "check", "c", "clippy", "test", "t", "bench", "run", "r",
                 "doc", "rustc", "rustdoc", "fix", "zigbuild"}
LEASE_FD = "JCODE_BUILD_LEASE_FD"


class Blocked(Exception):
    pass


class Short(Blocked):
    """Free space is below the request. Reclaiming build litter may fix it."""

    def __init__(self, message, free, needed):
        super().__init__(message)
        self.free, self.needed = free, needed


def reclaim(short):
    """Ask the host's build-litter collector for space once, outside the gate.

    `JCODE_BUILD_RECLAIM` names the command (empty disables). Without it the
    user-level `rust-disk-gc --guard <free GiB>` runs when installed. The
    collector only removes regenerable, lock-free build state.
    """
    configured = os.environ.get("JCODE_BUILD_RECLAIM")
    if configured == "":
        return False
    free_gib = short.free // GIB
    if configured:
        argv = [configured]
    else:
        default = Path.home() / ".local/bin/rust-disk-gc"
        if not os.access(default, os.X_OK):
            return False
        argv = [str(default), "--guard", str(free_gib)]
    env = {**os.environ, "JCODE_BUILD_FREE_GIB": str(free_gib),
           "JCODE_BUILD_NEEDED_GIB": str(-(-short.needed // GIB))}
    print(f"build admission: {free_gib} GiB free, {-(-short.needed // GIB)} GiB needed; "
          f"reclaiming build litter with {argv[0]}", file=sys.stderr)
    try:
        subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, stdout=sys.stderr,
                       timeout=int(os.environ.get("JCODE_BUILD_RECLAIM_TIMEOUT", "900")), check=False)
    except (OSError, subprocess.SubprocessError) as error:
        print(f"build admission: reclaim failed: {error}", file=sys.stderr)
        return False
    return True


def bounded_output(argv, env=None):
    result = subprocess.run(argv, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            stdin=subprocess.DEVNULL, timeout=10, check=False)
    if result.returncode:
        detail = result.stderr.decode(errors="replace").strip()
        raise Blocked(f"{argv[0]} could not resolve build storage: {detail or result.returncode}")
    return result.stdout


def existing(path):
    path = Path(path).resolve()
    while True:
        try:
            path.stat()
            return path
        except FileNotFoundError:
            path = path.parent


def pool_key(path, devices):
    device = path.stat().st_dev
    if device not in devices:
        key = f"device:{device}"
        if sys.platform == "darwin":
            source = bounded_output(["/bin/df", "-P", str(path)]).splitlines()[1].split()[0]
            if source.startswith(b"/dev/disk"):
                info = plistlib.loads(bounded_output(["/usr/sbin/diskutil", "info", "-plist",
                                                      os.fsdecode(source)]))
                if info["FilesystemType"] == "apfs":
                    key = "apfs:" + info["APFSContainerReference"]
        devices[device] = key
    return devices[device]


def cargo_action(args):
    iterator = iter(args)
    for arg in iterator:
        if arg == "--":
            break
        if arg in {"--config", "--color", "-Z", "-C"}:
            next(iterator, None)
        elif not arg.startswith(("-", "+")):
            return arg
    return None


def cargo_storage(cargo, args):
    metadata_args, toolchain, override = [], [], None
    iterator = iter(args)
    for arg in iterator:
        key, equals, value = arg.partition("=")
        if arg == "--":
            break
        if key == "-C" or key.startswith("-C"):
            raise Blocked("Cargo -C changes the build directory; invoke from that directory instead")
        if arg.startswith("+") and arg == args[0]:
            toolchain = [arg]
        if key in {"--manifest-path", "--config", "--target-dir", "-Z"}:
            value = value if equals else next(iterator)
            if key == "--target-dir":
                override = str(Path(value).resolve())
            else:
                metadata_args.extend((key, value))
        elif arg.startswith("-Z"):
            metadata_args.append(arg)
    env = os.environ.copy()
    env["RUSTUP_AUTO_INSTALL"] = "0"
    if override:
        env["CARGO_TARGET_DIR"] = override
    metadata = json.loads(bounded_output([
        cargo, *toolchain, "metadata", "--offline", "--no-deps", "--format-version", "1",
        *metadata_args,
    ], env))
    target = metadata["target_directory"]
    build = metadata.get("build_directory", target)
    if not Path(target).is_absolute() or not Path(build).is_absolute():
        raise Blocked("Cargo metadata did not give absolute target/build directories")
    return target, build


def read_policy(state):
    try:
        overrides = json.loads((state / "policy.json").read_text())
    except FileNotFoundError:
        return DEFAULT_POLICY
    if not isinstance(overrides, dict) or any(
        key not in DEFAULT_POLICY or type(value) is not int or not 0 <= value < 2**63
        for key, value in overrides.items()
    ):
        raise Blocked(f"invalid byte thresholds in {state / 'policy.json'}")
    return {**DEFAULT_POLICY, **overrides}


def demands(paths, auxiliary, policy, kind, units):
    devices = {}
    result = {}
    for candidates, amount, floor in (
        (paths, policy[kind] * units, policy["floor"]),
        (auxiliary, policy["aux"], policy["aux_floor"]),
    ):
        for path in candidates:
            ancestor = existing(path)
            pool = pool_key(ancestor, devices)
            demand = result.setdefault(pool, {"amount": 0, "floor": 0, "paths": {}})
            demand["amount"] = max(demand["amount"], amount)
            demand["floor"] = max(demand["floor"], floor)
            demand["paths"][str(Path(path).resolve())] = ancestor
    return result


def flock_now(fd):
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return True
    except OSError as error:
        if error.errno not in (errno.EACCES, errno.EAGAIN, errno.EWOULDBLOCK):
            raise
        return False


def file_fd(path, flags):
    fd = os.open(path, flags | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise Blocked(f"not a regular admission file: {path}")
    return fd


def lease_from(fd):
    record = json.loads(os.pread(fd, 8192, 0))
    if not isinstance(record["owner"], str) or not isinstance(record["pid"], int) or any(
        not isinstance(pool, str) or any(type(demand[key]) is not int or demand[key] < 0
                                         for key in ("amount", "floor"))
        for pool, demand in record["claims"].items()
    ):
        raise Blocked("invalid active lease record")
    return record


def inherited(state, requested):
    fd = int(os.environ[LEASE_FD])
    record = lease_from(fd)
    path = state / f"lease-{uuid.UUID(record['id'])}.json"
    with os.fdopen(file_fd(path, os.O_RDONLY), "rb") as live:
        if not os.path.samestat(os.fstat(fd), os.fstat(live.fileno())) or flock_now(live.fileno()):
            raise Blocked(f"inherited lease is not active: {path}")
    for pool, demand in requested.items():
        held = record["claims"].get(pool)
        if held is None or held["amount"] < demand["amount"] or held["floor"] < demand["floor"]:
            raise Blocked(f"inherited lease {path} from {record['owner']} does not cover "
                          f"{list(demand['paths'])} ({pool}, request={demand['amount']}, "
                          f"floor={demand['floor']})")


def reserve(state, requested, owner):
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    status = state.lstat()
    if not stat.S_ISDIR(status.st_mode) or status.st_uid != os.getuid() or status.st_mode & 0o077:
        raise Blocked(f"admission directory must be owned by this user and private: {state}")
    gate = state / "lock"
    with os.fdopen(file_fd(gate, os.O_RDWR | os.O_CREAT), "rb") as lock:
        deadline = time.monotonic() + 2
        while not flock_now(lock.fileno()):
            if time.monotonic() >= deadline:
                raise Blocked(f"admission transaction already in progress: {gate}")
            time.sleep(.05)
        active = []
        for path in state.glob("lease-*.json"):
            with os.fdopen(file_fd(path, os.O_RDONLY), "rb") as candidate:
                if flock_now(candidate.fileno()):
                    path.unlink()
                else:
                    active.append(lease_from(candidate.fileno()))
        for pool, demand in requested.items():
            stats = [os.statvfs(path) for path in set(demand["paths"].values())]
            free = min(info.f_bavail * info.f_frsize for info in stats)
            competitors = [lease for lease in active if pool in lease["claims"]]
            reserved = sum(lease["claims"][pool]["amount"] for lease in competitors)
            floor = max([demand["floor"], *(lease["claims"][pool]["floor"] for lease in competitors)])
            amount = demand["amount"]
            if free < floor + reserved + amount:
                owners = ", ".join(f"{x['owner']} (lease pid {x['pid']}, {x['claims'][pool]['amount']} B)"
                                   for x in competitors) or "none"
                raise Short(f"paths={list(demand['paths'])} pool={pool} free={free} B "
                            f"floor={floor} B request={amount} B reserved={reserved} B "
                            f"active owners: {owners}", free, floor + reserved + amount)
        lease_id = str(uuid.uuid4())
        path = state / f"lease-{lease_id}.json"
        fd = file_fd(path, os.O_RDWR | os.O_CREAT | os.O_EXCL)
        try:
            if not flock_now(fd):
                raise Blocked(f"cannot lock new lease {path}")
            claims = {pool: {key: demand[key] for key in ("amount", "floor")}
                      for pool, demand in requested.items()}
            data = json.dumps({"id": lease_id, "owner": owner, "pid": os.getpid(),
                               "claims": claims}).encode()
            if os.write(fd, data) != len(data):
                raise Blocked(f"incomplete lease {path}")
            os.set_inheritable(fd, True)
            os.environ[LEASE_FD] = str(fd)
        except BaseException:
            os.close(fd)
            raise


def execute(argv):
    os.execvp(argv[0], argv)


def cargo_proxy(cargo):
    justrust = shutil.which("justrust")
    path = shutil.which(cargo)
    return path is not None and (Path(path).resolve().name == "justrust" or
                                (justrust is not None and os.path.samefile(path, justrust)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--owner", required=True)
    parser.add_argument("--kind", choices=("checkout", "dev", "release", "desktop"), default="dev")
    parser.add_argument("--units", type=int, default=1)
    parser.add_argument("--path", action="append", default=[])
    parser.add_argument("--workspace", action="append", default=[])
    parser.add_argument("--cargo", action="store_true", help="resolve storage from the Cargo command")
    parser.add_argument("--check-inherited", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command and not args.check_inherited:
        parser.error("a command is required")
    policy = DEFAULT_POLICY
    try:
        if args.cargo:
            cargo_args = command[1:]
            action_args = cargo_args[:cargo_args.index("--")] if "--" in cargo_args else cargo_args
            if cargo_action(cargo_args) not in BUILD_ACTIONS or any(
                word in ("-h", "--help", "-V", "--version") for word in action_args
            ):
                execute(command)
            proxy_local = (os.environ.get("JUSTRUST_REMOTE") == "off" or
                           os.environ.get("JUSTRUST_CARGO_ROUTE") in ("0", "off", "false", "no"))
            if cargo_proxy(command[0]) and not proxy_local:
                # The proxy owns its hosted/default router. It has no supported pre-seeding hook.
                execute(command)
            if args.kind == "dev" and any(word in ("-r", "--release", "--profile=release", "release",
                                                    "--profile=release-lto", "release-lto")
                                          for word in action_args):
                args.kind = "release"
            args.path.extend(cargo_storage(command[0], cargo_args))
        for manifest in args.workspace:
            args.path.extend(cargo_storage("cargo", ["--manifest-path", manifest]))
        home = Path.home()
        state = home / ".local/state/jcode/build-admission"
        policy = read_policy(state)
        if not args.path or not 1 <= args.units <= 8:
            raise Blocked("expected resolved build paths and 1–8 build units")
        auxiliary = []
        if args.kind != "checkout":
            auxiliary.extend((os.environ.get("TMPDIR") or "/tmp",
                              os.environ.get("CARGO_HOME") or home / ".cargo",
                              os.environ.get("RUSTUP_HOME") or home / ".rustup"))
            auxiliary.extend(os.environ[key] for key in ("SCCACHE_DIR", "XDG_CACHE_HOME")
                             if os.environ.get(key))
        requested = demands(args.path, auxiliary, policy, args.kind, args.units)
        if LEASE_FD in os.environ:
            inherited(state, requested)
        elif args.check_inherited:
            raise Blocked("no inherited build lease")
        else:
            try:
                reserve(state, requested, args.owner)
            except Short as short:
                # Reclaim runs without the admission gate held: it can take
                # minutes, and competing reservations must stay responsive.
                if not reclaim(short):
                    raise
                policy = read_policy(state)
                requested = demands(args.path, auxiliary, policy, args.kind, args.units)
                reserve(state, requested, args.owner)
        if not args.check_inherited:
            execute(command)
    except (Blocked, OSError, ValueError, KeyError, IndexError, StopIteration, AttributeError, TypeError,
            RuntimeError, subprocess.SubprocessError, plistlib.InvalidFileException) as error:
        print(f"build admission blocked: owner={args.owner} cwd={Path.cwd()} "
              f"paths={args.path + args.workspace} policy={args.kind} "
              f"floor={policy['floor']} B request={policy[args.kind] * args.units} B: {error}",
              file=sys.stderr)
        return 75
    return 0


if __name__ == "__main__":
    sys.exit(main())
