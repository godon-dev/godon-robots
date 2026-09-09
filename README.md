# godon-robots

The robot runtime of godon: autonomous workers that tend live systems — probing, holding, walking, and optimizing on shared substrates, by lease and by turn.

## What a robot does

One robot = one lease-holder on a live system. Its day is a cycle of bounded trials:

- **Optimize** — own-work trials: apply a parameter suggestion, measure the effect, keep or roll back. Guardrails are checked before and after every push; rollback restores the prior state.
- **Walk** — characterization: sweep a parameter axis step by step to map the system's response curve. Walks feed the atlas — the connectome of the system.
- **Hold** — stand still during another robot's walk. Holds are cooperation, not budget spend: interference detection needs quiet receivers.
- **Pause** — rest at the neutral point between pushes; the target becomes a sensor, not an actuator.

The feeler protocol runs through all modes: touch gently (a bounded push), believe what answers (the measured effect), yield the turn (fair-share lease — acquire is denied while a walking peer has had fewer turns).

## Architecture

Trials are coordinated over an **Optuna database** (ask/tell storage, shared between robots). Effectuation and reconnaissance run as Windmill scripts on the target systems.

The system follows an **engine + strains** architecture: the engine (`engine/`) provides the generic trial loop — lifecycle, lease and turn-taking, algorithm diversity across parallel robots, guardrail checking, rollback, cooperative trial sharing, metrics — while strains (`strains/`) encapsulate domain-specific knowledge (parameter suggestion, validation).

### Engine (`engine/`)

- **SystemtenderWorker** — the robot loop: lifecycle management, lease citizenship, guardrail checking, rollback support
- **Communication** — cooperative trial sharing between robots via the shared Optuna store (probabilistic, best, worst, extremes strategies)
- **SystemtenderMetricsClient** — Prometheus metrics via Push Gateway
- **Strain loader** — dynamic loading and contract validation of strain modules

### Strains (`strains/`)

Each strain provides domain-specific logic as a pluggable module:
- `suggest_params(trial, settings)` — parameter suggestion for the next trial
- `validate_config(config)` — configuration validation (preflight checks)

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
Optimizes Linux system parameters (sysctl, sysfs, cpufreq, ethtool) for improved performance. Supports network, memory, CPU, and custom optimization objectives via Prometheus metrics.

### bench_greenhouse (`strains/bench_greenhouse/`)
Optimizes greenhouse climate simulation parameters. Supports multi-zone heating, ventilation, shading, CO2 injection, lighting, and irrigation. Designed for the `godon-bench-greenhouse` simulation container with HTTP-based effectuation and reconnaissance.

## License

AGPL-3.0
