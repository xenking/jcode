----------------------- MODULE RecoveryOwnership -----------------------
EXTENDS Naturals, TLC, RecoveryPolicy

VARIABLES owner, initialOwner, phase, observedOwner, reloadIntent,
          wasRunning, stoppedAfterRead

vars == <<owner, initialOwner, phase, observedOwner, reloadIntent,
          wasRunning, stoppedAfterRead>>

Init ==
    /\ owner \in {"reload", "historical", "missing"}
    /\ initialOwner = owner
    /\ phase = "selected"
    /\ observedOwner = FALSE
    /\ reloadIntent \in BOOLEAN
    /\ wasRunning \in BOOLEAN
    /\ stoppedAfterRead = FALSE

ReadOwner ==
    /\ phase = "selected"
    /\ observedOwner' = (owner \in LiveOwners)
    /\ phase' = "checked"
    /\ UNCHANGED <<owner, initialOwner, reloadIntent, wasRunning, stoppedAfterRead>>

StopCoordinator ==
    /\ phase = "checked"
    /\ owner = "reload"
    /\ owner' = "stopped"
    /\ stoppedAfterRead' = TRUE
    /\ UNCHANGED <<initialOwner, phase, observedOwner, reloadIntent, wasRunning>>

DecideContinuation ==
    /\ phase = "checked"
    /\ <<observedOwner, reloadIntent, wasRunning>> \in ResumeCases
    /\ phase' = "approved"
    /\ UNCHANGED <<owner, initialOwner, observedOwner, reloadIntent,
                   wasRunning, stoppedAfterRead>>

SkipContinuation ==
    /\ phase = "checked"
    /\ <<observedOwner, reloadIntent, wasRunning>> \notin ResumeCases
    /\ phase' = "skipped"
    /\ UNCHANGED <<owner, initialOwner, observedOwner, reloadIntent,
                   wasRunning, stoppedAfterRead>>

ClaimTurn ==
    /\ phase = "approved"
    /\ phase' = "claimed"
    /\ UNCHANGED <<owner, initialOwner, observedOwner, reloadIntent,
                   wasRunning, stoppedAfterRead>>

PublishRunning ==
    /\ phase = "claimed"
    /\ phase' = "published"
    /\ UNCHANGED <<owner, initialOwner, observedOwner, reloadIntent,
                   wasRunning, stoppedAfterRead>>

StartContinuation ==
    /\ phase = "published"
    /\ phase' = "started"
    /\ UNCHANGED <<owner, initialOwner, observedOwner, reloadIntent,
                   wasRunning, stoppedAfterRead>>

Done ==
    /\ phase \in {"started", "skipped"}
    /\ UNCHANGED vars

Next == ReadOwner \/ StopCoordinator \/ DecideContinuation \/ SkipContinuation
        \/ ClaimTurn \/ PublishRunning \/ StartContinuation \/ Done

TypeOK ==
    /\ owner \in {"reload", "historical", "missing", "stopped"}
    /\ initialOwner \in {"reload", "historical", "missing"}
    /\ phase \in {"selected", "checked", "approved", "claimed", "published", "started", "skipped"}
    /\ observedOwner \in BOOLEAN
    /\ reloadIntent \in BOOLEAN
    /\ wasRunning \in BOOLEAN
    /\ stoppedAfterRead \in BOOLEAN

HistoricalEvidenceCannotAuthorize ==
    (phase = "started" /\ initialOwner = "historical"
     /\ ~reloadIntent /\ ~wasRunning) => FALSE

StopAfterReadPreventsLaunch == (phase = "started") => ~stoppedAfterRead

ContinuationHasOwner == (phase = "started") => (owner = "reload")
=============================================================================
