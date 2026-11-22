#!/usr/bin/env python3
"""Exercise stock Cargo and the real macOS packaging entry point, without network."""

import os
from pathlib import Path
import platform
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
DESKTOP_PACKAGE = Path(sys.argv.pop(1)).resolve()


class BuildAdmission(unittest.TestCase):
    def test_entrypoints_share_reapable_lease(self):
        if sys.platform != "darwin":
            self.skipTest("macOS packaging entry point")
        rustup = shutil.which("rustup")
        cargo = subprocess.check_output([rustup, "which", "cargo"], text=True).strip()
        rustc = subprocess.check_output([rustup, "which", "rustc"], text=True).strip()
        with tempfile.TemporaryDirectory(prefix="jcode-admission-") as temp:
            temp = Path(temp)
            home, tools, cli, desktop, tmp = (temp / name for name in (
                "home", "tools", "jcode", "desktop", "tmp",
            ))
            for directory in (home, tools, cli, desktop, tmp):
                directory.mkdir()
            policy = home / ".local/state/jcode/build-admission/policy.json"
            policy.parent.mkdir(mode=0o700, parents=True)
            # Each invocation fits. A second would violate the shared floor.
            free = os.statvfs(temp).f_bavail * os.statvfs(temp).f_frsize
            margin = free // 10
            claim = free // 2 - margin
            floor = 3 * margin
            policy.write_text(
                f'{{"floor":{floor},"dev":{claim},"desktop":{claim},'
                f'"aux_floor":0,"aux":0}}'
            )

            def put(path, text, executable=False):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
                if executable:
                    path.chmod(0o755)

            for project, name, binary in ((cli, "jcode-harness-api-server", "jcode"),
                                          (desktop, "desktop", "jcode-desktop")):
                put(project / "Cargo.toml", f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2024"\n'
                    f'[[bin]]\nname = "{binary}"\npath = "src/main.rs"\n'
                    + ('[[bin]]\nname = "jcode-harness-api-bridge"\npath = "src/bridge.rs"\n'
                       if project == cli else ''))
                put(project / "src/main.rs", "fn main() {}\n")
            put(cli / "src/bridge.rs", "fn main() {}\n")
            put(cli / "build.rs", '''
use std::{env, fs, path::Path, thread, time::Duration};

fn main() {
    let Ok(hold) = env::var("FIXTURE_HOLD") else { return };
    let root = Path::new(&hold);
    if let Ok(fd) = env::var("JCODE_BUILD_LEASE_FD") {
        assert!(fs::metadata(format!("/dev/fd/{fd}")).is_ok());
    }
    fs::write(root.join("child"), std::process::id().to_string()).unwrap();
    while !root.join("release").exists() {
        thread::sleep(Duration::from_millis(50));
    }
}
''')
            (cli / "scripts").mkdir()
            (cli / "scripts/build-admission.py").symlink_to(ROOT / "scripts/build-admission.py")
            package = desktop / "scripts/package-macos.sh"
            package.parent.mkdir()
            shutil.copy2(DESKTOP_PACKAGE, package)
            put(desktop / "scripts/apple-network-retry.sh", "apple_network_retry() { \"$@\"; }\n")
            put(desktop / "scripts/fetch-sparkle.sh", '#!/bin/sh\nmkdir -p "$1/Sparkle.framework"\n', True)
            put(desktop / "scripts/render-macos-plist.py", "", True)
            put(desktop / "scripts/verify-macos-package.sh", "#!/bin/sh\nexit 0\n", True)
            put(desktop / "packaging/macos/Jcode.entitlements", "")
            for name in ("rustup", "codesign", "iconutil", "sips", "xcrun"):
                put(tools / name, "#!/bin/sh\nexit 0\n", True)
            put(tools / "lipo", '#!/bin/sh\ncp "$2" "$4"\n', True)
            put(tools / "ditto", '#!/bin/sh\nif [ "$1" = -c ]; then : >"$6"; else cp -R "$1" "$2"; fi\n', True)
            put(tools / "hdiutil", '#!/bin/sh\nfor arg do output=$arg; done\n: >"$output"\n', True)
            put(tools / "cargo", f'''#!/bin/bash
            if [[ "$1" == check && -n "${{FIXTURE_HOLD:-}}" ]]; then
                echo "$$" > "$FIXTURE_HOLD/cargo"
            fi
            exec {shlex.quote(cargo)} "$@"
            ''', True)

            env = dict(os.environ, HOME=str(home), CARGO_HOME=str(home / ".cargo"),
                       CARGO_NET_OFFLINE="true", RUSTC=rustc, TMPDIR=str(tmp),
                       GITHUB_ACTIONS="", JCODE_REMOTE_CARGO="0", JCODE_RUST_ACTION_LOG="0",
                       JCODE_CARGO_GATE="off", JCODE_SCCACHE="off", SCCACHE_DISABLE="1",
                       JCODE_CARGO_NICE="0", JCODE_REPO=str(cli), PACKAGE_TARGETS=(
                           "aarch64" if platform.machine() == "arm64" else "x86_64"
                       ) + "-apple-darwin",
                       PATH=f"{tools}:{os.environ['PATH']}")
            for key in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_BUILD_DIR",
                        "CARGO_BUILD_TARGET_DIR", "JCODE_BUILD_TMPDIR", "SPARKLE_ROOT",
                        "SPARKLE_ENV", "SPARKLE_PRIVATE_KEY", "SPARKLE_PUBLIC_KEY",
                        "APPLE_NOTARY_PROFILE", "APPLE_SIGNING_IDENTITY", "BASH_ENV",
                        "CARGO_TARGET_DIR", "JCODE_BUILD_LEASE_FD"):
                env.pop(key, None)
            dev_target, pkg_target, out = (temp / x for x in ("dev-target", "pkg-target", "out"))
            dev = [str(ROOT / "scripts/dev_cargo.sh"), "check", "--offline",
                   "--manifest-path", str(cli / "Cargo.toml"), "--target-dir", str(dev_target)]
            pack_env = dict(env, CARGO_TARGET_DIR=str(pkg_target), OUT_DIR=str(out))

            def run(command, cwd, run_env):
                return subprocess.run(command, cwd=cwd, env=run_env, capture_output=True,
                                      text=True, timeout=45, check=False)

            def assert_blocked(result):
                self.assertEqual(result.returncode, 75, result.stderr)
                for text in ("desktop-package", "dev_cargo", "floor=", str(pkg_target)):
                    self.assertIn(text, result.stderr)
                self.assertFalse(pkg_target.exists())
                self.assertFalse(out.exists())

            with (temp / "dev.log").open("w+") as log:
                proc = subprocess.Popen(dev, cwd=ROOT, env=dict(env, FIXTURE_HOLD=str(temp)),
                                        stdout=log, stderr=log, start_new_session=True)
                try:
                    for _ in range(160):
                        if (temp / "child").exists():
                            break
                        if proc.poll() is not None:
                            log.seek(0)
                            self.fail(log.read())
                        time.sleep(.05)
                    child = int((temp / "child").read_text())
                    cargo_pid = int((temp / "cargo").read_text())
                    assert_blocked(run([str(package)], desktop, pack_env))
                    metadata = run([str(ROOT / "scripts/dev_cargo.sh"), "metadata", "--offline",
                                    "--no-deps", "--format-version", "1", "--manifest-path",
                                    str(cli / "Cargo.toml")], ROOT, env)
                    self.assertEqual(metadata.returncode, 0, metadata.stderr)
                    proxy = temp / "proxy"
                    put(proxy / "justrust", "#!/bin/sh\necho routed-without-local-admission\n", True)
                    (proxy / "cargo").symlink_to("justrust")
                    routed = run(dev, ROOT, dict(env, PATH=f"{proxy}:{env['PATH']}",
                                                 JUSTRUST_REMOTE="force"))
                    self.assertEqual(routed.returncode, 0, routed.stderr)
                    self.assertIn("routed-without-local-admission", routed.stdout)
                    proc.kill()
                    proc.wait(timeout=5)
                    os.kill(child, 0)
                    os.kill(cargo_pid, 0)
                    assert_blocked(run([str(package)], desktop, pack_env))
                    (temp / "release").touch()
                    for _ in range(160):
                        if list(dev_target.rglob("*.rmeta")):
                            break
                        time.sleep(.05)
                    self.assertTrue(list(dev_target.rglob("*.rmeta")))
                    result = run([str(package)], desktop, pack_env)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertTrue((out / "Jcode.app/Contents/MacOS/jcode-desktop").is_file())
                    self.assertTrue((out / "SHA256SUMS").is_file())
                    policy.write_text("{")
                    invalid_target = temp / "invalid-target"
                    failed = run(dev[:-1] + [str(invalid_target)], ROOT, env)
                    self.assertEqual(failed.returncode, 75, failed.stderr)
                    self.assertIn("build admission blocked", failed.stderr)
                    self.assertFalse(invalid_target.exists())
                finally:
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    proc.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
