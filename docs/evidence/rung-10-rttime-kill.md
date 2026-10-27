# Rung 10 — RLIMIT_RTTIME 0 and the kernel's boost: a SIGKILL with no log line (2026-10-27)

Branch `fix/rttime` off `d0cf1f3`. Correctness only: no benchmark was run.

## The red gate

Gate 153 (`perf/handout-claims` `894b747`) failed the smoke row "ALTERNATING APPENDS FROM TWO MEMBERS TO ONE
FILE LOSE NOTHING — supervised" at A's first `umount`: "daemon … died without unmounting". The daemon left no
error line and no core (the key gate sets `RLIMIT_CORE` 0).

**Reproduced on dev too.** Loops of that row one at a time were green (15 on the branch, 15 on dev). The
compiled smoke binary for both arms, run in parallel loops (8 to 16 workers), failed about one run in 25 on
both trees: branch 2 of 80, dev 4 of 80, then 2 of 48, 2 of 60 and 4 of 128 on dev. The kept logs show
`supervisor: daemon N exited (wait status 0x9)`, with no stop line before it. In other runs B's daemon died the
same way, or the `squeezefs umount` verb itself ended "terminated with signal KILL".

## The cause

The tracefs event `signal_generate` was filtered to `sig == 9` with a stack trigger. Every kill was
`code=SI_KERNEL`, sent by the victim to itself from `posix_cpu_timers_work`: a CPU-timer rlimit at its hard
limit. Every process started from this desktop has `RLIMIT_RTTIME` 0/0. `/proc/<pid>/limits` along the chain:
systemd (1), the user manager, Hyprland and uwsm are unlimited; quickshell (the Omarchy shell) is 0/0, and so are
foot, fish and grok under it, and every agent's bash. `sudo` keeps the limit, so it reached every venue command,
the gate train's included.

`RLIMIT_RTTIME` bounds a real-time thread's CPU time between blocking calls. A CFS thread counts as real time
while the kernel boosts it: an RCU reader preempted past the boost delay (`rcub`, prio 98), or the owner of a
PI lock. Under a gate's load, a boosted squeezefs thread takes one real-time tick. The RT watchdog then arms
the CPU timer, and at a hard limit of 0 the process gets SIGKILL.

The bpftrace probe: `kprobe:task_tick_rt` printing the task (arg1) when its policy is 0 (a CFS task ticking
in the RT class, i.e. boosted), and `kprobe:posix_cpu_timers_work` printing the current task. During the
16 × 8 dev loop it recorded 4 boosted ticks at prio 98 (`sqz-fuse-ur3`, `sqz-fuse-ur27`, two `squeezefs` main threads). Each tick
was followed by one CPU-timer work and one SIGKILL. That loop had exactly 4 failed runs. Nothing in
`conveyor`/`smo` takes part.

## The fix

- **`cli.run`.** Every squeezefs process lifts `RLIMIT_RTTIME` to unlimited before its first thread
  (`cli/rt_limit.zig`). The daemon and the supervisor are forks of `mount` and inherit it. Without
  CAP_SYS_RESOURCE, a finite hard limit stays in place: the soft limit is raised to it, and `mount` warns with
  the value.
- **`scripts/venue` local.** Every command, and the interactive shell, runs under `ulimit -R unlimited`. This
  covers the test runner, its shells and zig, which the same boost can kill. The lift runs inside `sudo`'s
  root shell, because an unprivileged shell under a hard 0 cannot raise it. If the kernel refuses the lift
  (root without CAP_SYS_RESOURCE), the command prints `venue: WARNING: RLIMIT_RTTIME stays at …` and still
  runs. Both cases were checked: from this desktop's shell, and under `capsh --drop=cap_sys_resource`.

**Red** (cli "every squeezefs process lifts RLIMIT_RTTIME …", `05fbf6e`). A forked child starts at 0/0, and
another at a finite soft limit under an unlimited hard one. Both must end unlimited. The test was red before
the fix and green after it.

**After** (`097acfb`, venue not yet lifted, so the processes started at 0/0). The same 16 × 8 parallel loop
passed 128 of 128. bpftrace recorded 5 boosted ticks at prio 98 in squeezefs processes (`sqz-meta0` twice,
`squeezefs` main thread three times) and no CPU-timer work, so there were no kills.

The smoke's "EXTERNAL KILL … a concurrent runner swept the venue" verdict (a SIGKILL the test never sent) is
also what this kill looks like. Skips under that verdict before this fix may have been this kill.
