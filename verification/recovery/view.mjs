export const title = "Worker recovery can outlive its coordinator";
export const state = (v) => {
  if (v.phase === "selected") return "Recovery candidate";
  if (v.phase === "skipped") return "Worker stays idle";
  if (v.phase === "started") {
    if (v.stoppedAfterRead) return "Worker starts after coordinator stop";
    if (v.owner !== "reload") return "Worker starts without an owner";
    return "Worker resumes for its coordinator";
  }
  const admission = {
    approved: "Continuation approved",
    claimed: "Worker turn reserved",
    published: "Worker marked running",
  }[v.phase];
  if (admission) {
    if (v.stoppedAfterRead) return `${admission} after coordinator stop`;
    if (v.owner !== "reload") return `${admission} without an owner`;
    return admission;
  }
  if (v.stoppedAfterRead) return "Coordinator stopped after the ownership check";
  if (v.owner === "historical") return "Only historical ownership evidence";
  if (v.owner === "missing") return "No coordinator found";
  return "Coordinator was live before reload";
};
export const step = (action, from, to) => ({
  ReadOwner: "recovery reads the coordinator",
  StopCoordinator: "the user stops the coordinator",
  DecideContinuation: to.reloadIntent
    ? "a current reload intent approves work"
    : to.wasRunning
      ? "previously running work is approved"
      : "unfinished work is approved",
  ClaimTurn: "recovery reserves the worker turn",
  PublishRunning: "the worker is marked running",
  StartContinuation: "recovery starts the continuation",
  SkipContinuation: "recovery leaves the worker idle",
  Done: "recovery finishes",
}[action] ?? action);
