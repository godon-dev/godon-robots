# Godon Roadmap

## Phase 0: Engine Works End to End

The optimization loop must run reliably before anything else.

- [ ] Full ask → suggest → effectuate → reconnoiter → tell cycle runs without errors
- [ ] Multi-worker diversity (different samplers) verified
- [ ] Guardrails trigger correctly on limit violations
- [ ] Rollback recovers from consecutive failures
- [ ] Completion criteria (iterations, time budget, quality) stop breeders as configured
- [ ] Cooperative communication shares trials between workers/breeders
- [ ] Metrics flow to Prometheus push gateway
- [ ] End-to-end integration test against a real target (or realistic stub)

## Phase 1: Extract Targets as First-Class Resource

Cheap CRUD -- copy the credentials pattern. No discovery yet.

### godon-controller

- [ ] `controller/target_*.py` scripts (create, get, list, delete) -- same shape as credential scripts
- [ ] `targets` table in metadata DB: `id, name, type, address, credential_ref, tags[], metadata JSONB, created_at`
- [ ] Config validation: accept `targetRefs: [id]` in breeder config as alternative to embedded targets
- [ ] Breeder service resolves target refs at creation time
- [ ] Backward compatible: embedded targets still work, targetRefs is opt-in

### godon-images/godon-api

- [ ] `Target`, `TargetCreate`, `TargetSummary` types in `types.rs`
- [ ] Routes: `GET /targets`, `POST /targets`, `GET /targets/{id}`, `DELETE /targets/{id}`
- [ ] `windmill_adapter.rs`: proxy calls to `f/controller/target_*` scripts

### godon-images/godon-mcp

- [ ] MCP tools for target CRUD (list, create, get, delete)

### godon-breeders

- [ ] Config schema documentation update
- [ ] No engine changes -- engine receives resolved targets from controller

### Migration

- [ ] Phase 1a: targets CRUD exists, breeder config unchanged
- [ ] Phase 1b: breeder creation accepts `targetRefs`, resolves to inline targets for workers
- [ ] Phase 1c: deprecate embedded targets in config docs (still functional)

## Phase 2: Auto-Discovery (When It Earns Its Complexity)

Per-strain discovery that queries the target directly.

### Strain contract extension

```
current:  suggest_params(trial, settings) → params
          validate_config(config) → valid

extended: discover(target) → {parameters, current_values, valid_ranges}
```

### Implementation per strain

`linux_performance` discovery scripts:

- [ ] `strains/linux_performance/discover.py` with `discover(target)` entry point
- [ ] sysctl: SSH `sysctl -a` → intersect with PARAMETER_REGISTRY
- [ ] sysfs: SSH `ls /sys/class/net/` → build available NIC list
- [ ] cpufreq: SSH read `scaling_available_governors`
- [ ] ethtool: SSH `ethtool -k <iface>` → intersect with registry
- [ ] Returns config-space JSON suitable for building a breeder's `settings` section

### API surface

- [ ] `POST /targets/{id}/discover?strain=linux_performance` in godon-api
- [ ] Controller script to orchestrate: resolve target → get credential → call strain discovery
- [ ] MCP tool: `discover_target(target_id, strain)`

### When to start

- When template drift is a real user pain, not a hypothetical one
- When the engine is proven reliable across multiple breeding runs
- When at least one more strain exists (confirms the per-strain contract scales)

## Notes

- Targets are not a CMDB. Flat table with tags. No health polling, no agents, no drift detection.
- Tags handle grouping (`["prod", "cluster-a", "database-tier"]`). Richer hierarchy only if it hurts.
- Each phase is independently deployable. No big bang.
- Discovery is "read instead of write" using the same SSH path effectuation already uses.
