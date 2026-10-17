# godon-robots

The robot runtime of godon: autonomous workers that tend live systems — probing, holding, and walking on shared substrates, by lease and by turn.

## What a robot does

One robot = one lease-holder on a live system. Its day is a cycle of bounded trials:

- **Walk** — own-work trials: sweep a parameter axis step by step to map the system's response curve. Walks feed the atlas — the connectome of the system. Guardrails are checked before and after every push; rollback restores the prior state.
- **Hold** — stand still during another robot's walk: interference detection needs quiet receivers.
- **Pause** — rest at the neutral point between pushes; the target becomes a sensor, not an actuator.

The feeler protocol runs through all modes: touch gently (a bounded push), believe what answers (the measured effect), yield the turn (fair-share lease — acquire is denied while a walking peer has had fewer turns).

## Architecture

Trials are recorded in the engine's **ledger** — tables in the tender's own Postgres database, one connection per operation, shared between robots. Effectuation and reconnaissance run as Windmill scripts on the target systems: one-line header scripts with empty bodies, where the engine binary picks its component from the job path and reads its own job arguments. Walk policies read the causal notebook page for the system group to choose the next probe level.

The system follows an **engine + strains** architecture: the engine (`engine/`) provides the generic trial loop — lifecycle, lease and turn-taking, guardrail checking, rollback, metrics, and the seek lane — while strains (`strains/`) encapsulate domain-specific knowledge (parameter registries, preflight validation, per-dial suggestion logic). The `wmill/` crate is the Windmill client (run script by path, set state, get variable).

### Engine (`engine/`)

- **SystemtenderWorker** — the robot loop: lifecycle management, lease citizenship, guardrail checking, rollback support, wish serving — a held wish pins the tender past walk-complete, and every exit reverts held dials to neutral
- **ProbeCoordinator** — the detection/characterization loop: decides what to probe next, and when a curve is done
- **Characterization** — response curve interpolation and convergence detection
- **WalkPolicy** — stateless walk policy over the causal notebook page: picks the next level from banked measurements
- **CoverageWalk** — deterministic coverage walk over per-parameter level spaces
- **Seek wish** — declared intent that overrides the walk's priorities; two forms, one grammar: `cover_now` walks a declared region first, `find_best` searches it under a budget and returns the best found
- **Seek driver** — one question per trial: the worker's loop asks it what the next trial does, then effectuates, measures, and hands the reading back
- **SystemtenderMetricsClient** — Prometheus metrics via Push Gateway
- **Strain loader** — dynamic loading and contract validation of strain modules

### Strains (`strains/`)

Each strain provides domain-specific logic as a pluggable module:
- `validate_config(config)` — configuration validation (preflight checks)
- `preflight` — create-time check the controller's executor calls through Windmill (linux_performance, bench_greenhouse, bench_microgrid; exposed as the strain's `preflight` script)

### Effectuation (`effectuation/`)

Scripts that apply parameter changes to target systems. Each script follows the `(context, targets, settings)` interface contract:
- `context` — static robot run configuration (credentials, URLs, playbook paths)
- `targets` — list of target systems to apply changes to
- `settings` — the suggested parameters for this trial

Available effectuators:
- **SSH** — applies configuration via Ansible playbooks over SSH
- **HTTP** — applies configuration via HTTP API calls (draft)

### Reconnaissance (`reconnaissance/`)

Scripts that gather metrics to evaluate trial outcomes. Same `(context, targets, settings)` interface contract. Supports:
- **Prometheus** — multi-sample collection, stabilization waits, and aggregation
- **HTTP** — metric collection via HTTP GET with configurable stabilization, multi-sampling, and aggregation

## Available Strains

### linux_performance (`strains/linux_performance/`)
Tends Linux system parameters (sysctl, sysfs, cpufreq, ethtool). Supports network, memory, CPU, and custom objectives via Prometheus metrics.

### bench_greenhouse (`strains/bench_greenhouse/`)
Tends greenhouse climate simulation parameters. Supports multi-zone heating, ventilation, shading, CO2 injection, lighting, and irrigation. Designed for the `godon-bench-greenhouse` simulation container with HTTP-based effectuation and reconnaissance.

### bench_generic (`strains/bench_generic/`)
Domain knowledge for a configurable synthetic coupling bench: parameter registry and suggestion logic, used as validation ground truth.

### bench_microgrid (`strains/bench_microgrid/`)
Domain knowledge for microgrid simulation: parameter registry, preflight validation, and suggestion logic for power grid management.

## License

AGPL-3.0
