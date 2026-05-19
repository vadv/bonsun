# Bundle Architecture Research for bosun-client

Research date: 2026-05-19
Author: research-agent
Status: draft for design-doc input

## Scope and method

Reference tools surveyed: Helm 3 (Kubernetes), Ansible (roles and collections), Salt (formulas + pillar), Chef (cookbooks + Berkshelf + Policyfile), Puppet (modules + Hiera 5), Terraform (modules + provider lock), Skycfg (Stripe), Bazel (Starlark `load`, visibility), NixOS (flakes + sops-nix / agenix), Pulumi (component resources).

Method: primary-doc reads + 2025/2026 retros and tooling posts. Lens applied: rust-architecture (small explicit boundaries), schema-migration-safety (phased rollout), monitoring (observable failure modes), building-cicd-pipelines (signed-artifact distribution).

User-fixed constraints respected: signed tar.gz distribution, role-based layout, explicit Starlark inventory loading, tags filter loading, per-role templates, bundle semver decoupled from binary.

## 1. Comparative matrix

| Tool | Distribution | File structure | Dependency mechanism | Inventory / values layering | Multi-env | Signing | Key pain points |
|---|---|---|---|---|---|---|---|
| Helm 3 | `.tgz` over HTTP repo OR OCI registry (default 3.8+) | `Chart.yaml`, `values.yaml`, `templates/`, `charts/` (subcharts), `crds/`, `.helmignore` | `dependencies:` in `Chart.yaml`; `Chart.lock`; subcharts vendored in `charts/` | Single `values.yaml` + `--values` overlay; `--set` last-wins; subcharts read `.Values.<subchart>` | `-f base.yaml -f prod.yaml` overlay pattern | provenance file (deprecated PGP) OR cosign/Sigstore on OCI | Subchart override warnings ("cannot overwrite table"); two subcharts pinning different versions of same dep override each other; subchart cannot read parent values; lookup function violates idempotency [1, 2, 3] |
| Ansible roles | `tar.gz` via Galaxy or git | `tasks/`, `handlers/`, `defaults/`, `vars/`, `templates/`, `files/`, `meta/`, `library/` | `meta/main.yml` `dependencies:`; pulled before role runs | 22-level variable precedence; `defaults/` lowest, `extra_vars` highest | `group_vars/` + `host_vars/` per inventory dir | `ansible-sign` (detached GPG) | Tag inheritance differs between `import_role` (propagates) and `include_role` (does NOT propagate) — silent footgun [4] |
| Ansible collections | `tar.gz` via Galaxy / Automation Hub | `galaxy.yml`, `plugins/`, `roles/`, `playbooks/`, `requirements.yml` | `requirements.yml`; Pulp 3 server resolves | Same as roles | Same as roles | Pulp 3 detached GPG signatures | Migration churn 2.10→collections fragmented community; single-role repos no longer first-class [5] |
| Salt formulas | git clone / file_roots | `formula-name/init.sls`, `map.jinja`, `pillar.example` | Implicit via `include:` in SLS; no version pinning | Pillar (top.sls per env) + grains (host facts) | `saltenv` + `pillarenv`; default merges all envs → confusing | None official; rely on git refs | Pillar render is single-master bottleneck; >1000 pillar files = 95% CPU on master, ~250 minion cap [6] |
| Chef cookbooks | `.tgz` from Supermarket; Policyfile lock | `recipes/`, `attributes/`, `templates/`, `files/`, `libraries/`, `resources/`, `metadata.rb` | Berkshelf (deprecated) → Policyfile (immutable lock) | 15-level attribute precedence with `force_*` and `!` variants | Environments + Policyfile groups | InSpec/Chef Automate signing | 15-level precedence is the canonical "do not do this" example; teams aggressively dropped `normal` level [7, 8] |
| Puppet modules | `.tar.gz` from Forge; Puppetfile via r10k/Code Manager | `manifests/`, `templates/`, `files/`, `data/`, `metadata.json`, `examples/` | `metadata.json` `dependencies:`; r10k Puppetfile NOT auto-resolved transitively | Hiera 5 layered hierarchy; module-level + env-level hiera.yaml | Hiera env tiers + facts | Forge has GPG signing | Hiera lookup chain debugging is notoriously hard; soft vs hard deps split between README and metadata.json [9] |
| Terraform modules | git/registry; **no module lockfile** | `main.tf`, `variables.tf`, `outputs.tf`, `modules/<sub>`, `examples/<ex>` | `module {}` block with `source` + `version` constraint | `terraform.tfvars`, `*.auto.tfvars`, `-var-file`, `TF_VAR_*` env, CLI `-var` | `terraform workspace` (state-level) or per-env dirs | `.terraform.lock.hcl` locks **providers only**; not modules | "Always picks newest matching module version" unless exact pin; lock file is provider-only [10] |
| Skycfg (Stripe) | embedded library; no archive format | Starlark `.star` files + protobuf schemas; `load("//path:file")` | Direct `load()` graph; custom load handlers | Caller passes `ctx` dict | App-defined; not built-in | None | No standalone distribution format; pre-1.0 due to internal protobuf coupling [11] |
| Bazel/Buck2 | workspace local; bzlmod modules | `BUILD.bazel`, `*.bzl`, `MODULE.bazel`, `WORKSPACE` | `MODULE.bazel` with bzlmod, semver-ish resolution | `--define`, `select()` on platforms | `select()` config_setting | `MODULE.bazel.lock` | Load visibility added only in Bazel 6.0; `_underscore` private symbols are the only sub-file boundary [12] |
| NixOS modules | flake registry (git); content-hashed | `flake.nix`, `modules/`, `overlays/` | `inputs = { ... }` in flake, locked in `flake.lock` | Module composition; `lib.mkMerge`, `lib.mkOverride` priorities | per-host config + shared modules | flake.lock content hash; secrets via agenix (age) or sops-nix (age/gpg) | Secrets handling is a known sharp edge; multiple competing solutions (agenix, sops-nix, nixos-artifacts) [13] |
| Pulumi components | language packages (npm/PyPI) OR Pulumi packages via `pulumi package add` | language-native; ComponentResource class | language package manager + plugin descriptor | program code | program code + stack config | plugin signatures | Three competing distribution models; cross-language components newer, still maturing [14] |

## 2. Top 10 architectural problems any new bundle format must avoid

1. **Precedence hierarchies > 5 levels are user-hostile.** Chef's 15 attribute tiers with `force_*` and `!` variants are widely regretted; practitioners deprecate `normal` and reduce to default + role-level + env-level. Ansible's 22-level variable precedence is similarly debugged by `ansible -m debug -a "var=foo"` rather than understood [7, 8].

2. **Subchart / sub-module value override that the child cannot see.** Helm's "subcharts cannot read parent values, but parents must namespace overrides under subchart name" yields "cannot overwrite table with non-table" warnings and silent merge mistakes [2, 3].

3. **Two consumers of the same sub-dependency at different versions.** Helm subcharts overwrite each other's template functions when this happens — bug 11933, unresolved [3]. Diamond dependency at template level, not just code.

4. **Tag semantics that differ between `import` and `include`.** Ansible's `import_role` propagates tags to all tasks; `include_role` does NOT. Same syntax, opposite behavior. Source of long-running confusion [4].

5. **Single-master pillar compile bottleneck.** Salt master spends 95% CPU compiling pillar at >1000 pillar files, caps at ~250 minions concurrent without pillar cache [6]. Centralized inventory compilation does not scale linearly.

6. **Implicit auto-merge of all environments.** Salt's default merges pillar data from all saltenvs unless `pillarenv` is pinned. Users get production secrets in staging runs by accident [15].

7. **Lock file scope confusion.** Terraform's `.terraform.lock.hcl` locks providers only, not modules. Users assume "init then commit lock" gives reproducibility; module version drift is silent [10].

8. **Signing afterthought.** Helm's original PGP provenance is deprecated; OCI + cosign is the new path, but the migration leaves a hole where unsigned charts are normal. Helm Issue 10644 ("Helm supply chain security") sat open for years. Don't ship without signing on day 1 [16, 17].

9. **Distribution format coupled to runtime details.** Skycfg cannot reach 1.0 because it depends on `go-protobuf` internals; Berkshelf is deprecated in favor of Policyfile mostly because the resolver model leaked Ruby semantics [11, 18]. Keep the on-disk format orthogonal to the interpreter's internal representation.

10. **Secrets in plaintext values.** Helm + values.yaml + git → secrets in repo, in tarball, in chart cache. Industry response: ExternalSecrets operator / Vault / sops / agenix — external store referenced by name, never inlined [19, 13]. A bundle must never carry decryptable secret material; only references.

## 3. Proposal for bosun bundle architecture

### 3.1 Directory layout

```
mybundle/
├── bundle.toml                      # bundle manifest (root, required)
├── bundle.lock                      # resolved versions (generated, committed)
├── CHANGELOG.md                     # bundle change log
├── README.md                        # optional, human docs
├── .bosunignore                     # globs excluded from archive (like .helmignore)
├── manifests/
│   └── main.star                    # bundle entry point, orchestrates roles
├── roles/
│   ├── postgres/
│   │   ├── role.toml                # role metadata (name, version, requires_role)
│   │   ├── main.star                # role entry; declares resources
│   │   ├── lib.star                 # optional internal helpers (private)
│   │   ├── templates/               # role-local templates only
│   │   │   ├── postgresql.conf.j2
│   │   │   └── pg_hba.conf.j2
│   │   └── inventory/               # role-default inventory, loaded by tag
│   │       ├── _base.yaml
│   │       ├── production.yaml
│   │       └── staging.yaml
│   ├── pgbouncer/
│   │   └── ... (same shape)
│   └── patroni/
│       └── ...
└── inventory/                       # cross-role / cluster-level inventory
    ├── _base.yaml
    ├── production.yaml
    └── staging.yaml
```

Rationale:
- Roles are first-class directories — proven by Ansible roles, Puppet modules, Chef cookbooks. They map to the postgres-chiit reality (16 roles, each with own templates).
- Role-local `templates/` eliminates the Helm-subchart-parent-templates resolution problem. A role only sees its own templates by default.
- Role-local `inventory/` and bundle-level `inventory/` together: roles ship sensible defaults; bundle-level overrides apply per-cluster.
- No shared `templates/` at bundle root — explicitly excluded per user constraint.

### 3.2 `bundle.toml` schema

```toml
[bundle]
name           = "postgres-cluster"   # required, lowercase, no underscores
version        = "1.4.0"              # required, semver
description    = "Postgres + Patroni + pgbouncer fleet bundle"
requires_bosun = "^0.3"               # required, agent semver constraint
entry          = "manifests/main.star" # required

# Roles declared in this bundle, with their pinned versions.
# Each role lives in roles/<name>/ and has its own role.toml.
[[bundle.roles]]
name    = "postgres"
version = "2.1.0"

[[bundle.roles]]
name    = "patroni"
version = "1.0.3"

[[bundle.roles]]
name    = "pgbouncer"
version = "0.9.1"

# Tags available for --tags filter. Documented contract.
# Bundle author registers them; main.star checks bosun.tags has X.
[bundle.tags]
production = "Production cluster, real workload"
staging    = "Staging cluster, may be destroyed"
canary     = "Subset of production for testing risky changes"

# Optional: signing metadata, populated at build time.
[bundle.signing]
algorithm = "cosign-keyless"   # or "cosign-key", "minisign"
oidc_issuer = "https://accounts.google.com"  # for keyless
identity = "ops@example.com"
```

### 3.3 Role concept

A role is a self-contained unit:
- Owns its templates (`roles/<name>/templates/`).
- Owns its default inventory (`roles/<name>/inventory/`).
- Has its own version (`role.toml`) — independent of bundle semver.
- Declares cross-role dependencies in `role.toml` with `requires_role` constraints (max one level deep — see open question 5).

```toml
# roles/patroni/role.toml
[role]
name    = "patroni"
version = "1.0.3"

[[role.requires_role]]
name    = "postgres"
version = "^2.0"

# Role-private symbols: names starting with `_` cannot be load()-ed from outside the role.
# Borrowed from Bazel/Starlark convention [12].
```

Cross-role usage uses an explicit symbol API, no global state sharing:

```python
# manifests/main.star
load("@roles/postgres", postgres_install = "install", postgres_config = "configure")
load("@roles/patroni", patroni_install = "install")

postgres_install(version = "16")
patroni_install(scope = "main-cluster")
```

### 3.4 Inventory loading (explicit Starlark API)

Inventory is loaded **explicitly** in `main.star`. No auto-scan of `defaults/`. Order is the author's responsibility. Tags filter the load:

```python
# manifests/main.star
load("@bosun/builtins", "inventory", "tags")

# Always-loaded base
inv = inventory.load("inventory/_base.yaml")

# Tag-gated overlays. tags.has() reads from --tags CLI arg.
if tags.has("production"):
    inv = inventory.merge(inv, inventory.load("inventory/production.yaml"))
if tags.has("staging"):
    inv = inventory.merge(inv, inventory.load("inventory/staging.yaml"))

# Per-role inventory: role chooses when to merge. No magic auto-include.
load("@roles/postgres", postgres_role = "role")
postgres_role(inventory = inv)
```

API signatures:

```python
inventory.load(path: str) -> dict
    # Reads YAML relative to bundle root, returns dict. Errors if file missing.

inventory.merge(*sources: dict, strategy: str = "deep") -> dict
    # Deep-merge in argument order. Last-wins for scalars, recursive for dicts,
    # configurable for lists ("replace" | "append" | "error_on_conflict").

tags.has(tag: str) -> bool
    # Returns True iff CLI received --tags=...,tag,...

tags.require_one_of(*tags: str)
    # Raises if none of the given tags is active. Used to enforce env selection.

tags.active() -> list[str]
    # Returns sorted list of active tags. For logging only.
```

### 3.5 Tag semantics — exact behavior

CLI: `bosun apply --tags=production,canary mybundle.tar.gz`

Rules:
1. Tags is a **set of strings**. Order on CLI does not matter.
2. `tags.has(x)` returns True iff x is in the set.
3. **No tag inheritance.** Unlike Ansible, calling a role's function does not propagate tags into the role automatically. The role's own `main.star` checks `tags.has()` if it cares. This avoids the Ansible `import_role` vs `include_role` footgun [4].
4. If no `--tags` flag is given, the set is empty. `main.star` MUST call `tags.require_one_of("production", "staging", ...)` near the top to fail fast — this prevents the "Salt default-merge-all-envs" trap [15].
5. Tags MAY appear in `bundle.toml` `[bundle.tags]` for documentation; the runtime does not enforce that only declared tags are passed (extensibility for custom feature flags).

### 3.6 Per-role template resolution

`template("postgresql.conf.j2")` resolution **within a role** follows:

1. `roles/<calling-role>/templates/postgresql.conf.j2`  ← only location

There is no fallback to a bundle-root `templates/` because the user constraint forbids it. The role context is determined by which role's `main.star` is currently executing; the Starlark interpreter tracks the current role through the `load()` chain (Skycfg-style file context [11], with role boundary).

Cross-role template access is **forbidden by default**:
- `roles/postgres/main.star` cannot call `template("patroni/foo.j2")`.
- If a role needs to expose a template to others, it must call the explicit API `role.expose_template("foo.j2")` which makes it loadable via `template("@roles/postgres:foo.j2")` from outside. Default-private, opt-in public — Bazel visibility model [12].

### 3.7 Distribution: archive layout, signing, manifests

Archive format: `<name>-<version>.bosun.tar.gz`. Inside the tarball:

```
postgres-cluster-1.4.0/
├── bundle.toml
├── bundle.lock
├── MANIFEST                # generated, see below
├── manifests/...
├── roles/...
└── inventory/...
```

`MANIFEST` file (generated at build, verified at apply):

```
# bosun bundle manifest v1
bundle: postgres-cluster
version: 1.4.0
created: 2026-05-19T10:00:00Z
files:
  bundle.toml                                sha256:abc123...
  bundle.lock                                sha256:def456...
  manifests/main.star                        sha256:...
  roles/postgres/role.toml                   sha256:...
  roles/postgres/main.star                   sha256:...
  roles/postgres/templates/postgresql.conf.j2 sha256:...
  ...
```

Signing pipeline:

1. Build produces `<name>-<version>.bosun.tar.gz`.
2. `sha256(tarball)` = bundle digest. This is the canonical bundle identity.
3. Cosign signs the digest: `cosign sign-blob --output-signature <name>-<version>.bosun.tar.gz.sig <name>-<version>.bosun.tar.gz`. Keyless via Sigstore OIDC, OR keyed via team key. Pattern follows the Helm-on-OCI-with-cosign workflow [17, 16].
4. `MANIFEST` inside the archive lets apply-time verification check individual file integrity AFTER signature verification (defense in depth).

Verification on `bosun apply`:
1. Verify signature over tarball digest.
2. Unpack into per-bundle staging dir (content-addressed by digest, NOT bundle name+version — protects against version-string-reuse).
3. Verify each file in `MANIFEST` against actual content. Fail closed.
4. Refuse to load if `MANIFEST` is missing OR has files not listed OR lists files not present.

Distribution channel:
- Phase 1: HTTPS file server (bosun-server) serving `<name>/<version>/<name>-<version>.bosun.tar.gz` + `.sig`.
- Phase 2 (when bosun-server matures): OCI registry. Push `oras push registry/path/bundle:1.4.0 --artifact-type application/vnd.bosun.bundle.v1+tar.gz <tarball>`. Cosign verification via standard OCI tooling [20].

### 3.8 Multi-bundle on one node

Two independent bundles on one node share **no state by default**. Each bundle:
- Resolves its own role set.
- Resolves its own templates and inventory.
- Writes resource state to a per-bundle path: `/var/lib/bosun/state/<bundle-name>/`.

Cross-bundle resource conflicts (two bundles managing `/etc/postgresql/postgresql.conf`):
- Detected at apply: each bundle writes its claim to `/var/lib/bosun/claims/<absolute-path>` on first apply.
- A second bundle claiming the same path **fails** with "already claimed by bundle X v.Y" unless explicit `--allow-claim-takeover` flag is given.
- This matches the rust-architecture lens: explicit ownership over implicit shared state, no `Arc<Mutex<_>>`-style global registries [21, internal].

Composition story for future: a `dependencies` section in `bundle.toml` declaring soft links to other bundles; out of scope for v1.

### 3.9 Versioning

| Versioned thing | Constraint syntax | Where declared |
|---|---|---|
| Bundle | semver, e.g. `1.4.0` | `bundle.toml` `[bundle].version` |
| Bundle → bosun agent | semver range, e.g. `^0.3` | `bundle.toml` `[bundle].requires_bosun` |
| Bundle → role | exact version | `bundle.toml` `[[bundle.roles]]` |
| Role → role | semver range, e.g. `^2.0` | `roles/<name>/role.toml` `[[role.requires_role]]` |
| Bundle conflict | bundle name | `bundle.toml` `[bundle].conflicts_with = ["legacy-postgres"]` (future) |

The bundle declares **exact** role versions (Policyfile model [18]) — no resolver-at-apply-time. Resolution happens at build (`bosun bundle build` reads role.toml constraints, picks versions, writes `bundle.lock`). At apply the lock is authoritative; the agent does not resolve.

This sidesteps:
- Helm's "two subcharts pick different versions of the same dep" template clash [3].
- Terraform's "module-version is not in the lock file" silent drift [10].
- Chef's Berkshelf resolver flakiness that led to its deprecation [18].

### 3.10 Local dev vs production

Apply targets a path or an OCI/HTTPS URL:

```
bosun apply ./mybundle/                            # local source, unsigned, --allow-unsigned
bosun apply ./mybundle.tar.gz                      # local archive, must be signed unless --allow-unsigned
bosun apply https://bosun-server/api/bundles/postgres-cluster/1.4.0  # signed required
bosun apply oci://registry.example/bosun/postgres-cluster:1.4.0      # signed required
```

`--allow-unsigned` is dev-only; production agent config sets `allow_unsigned = false` in its own config, refusing the flag.

## 4. Open questions (to resolve in spec)

1. **Role isolation strength.** Is the Starlark interpreter sandboxed per role (separate global namespace), or is it one interpreter with file boundaries? Per-role interpreters give clean isolation but cost performance; shared interpreter with `_underscore` privates is faster but leakier.

2. **Inventory list-merge strategy.** Deep-merge dicts is well-defined. Lists are not. Options: replace (last wins), append (additive), key-by (merge by some `id` field), error_on_conflict. Pick a single default; expose strategy as `inventory.merge(strategy = "replace")` argument.

3. **Cross-role template sharing.** Section 3.6 proposes opt-in `expose_template`. Is this ever needed in practice, or should cross-role templates be banned outright and forced into a Starlark string returned by a role function?

4. **Inventory secrets.** Bundle MUST NOT carry decryptable secrets. But how does inventory reference a secret? Options: (a) bundle declares `secret_ref = "vault:secret/data/postgres#password"` and bosun-client has a secrets provider plugin; (b) bundle uses a placeholder, bosun-server injects at fetch time; (c) bundle defers to k8s secrets / file-on-disk by absolute path. This decision blocks the secret-handling story end-to-end [19, 13].

5. **Cross-role dependency depth.** `role A → role B → role C` is technically allowed but who orchestrates the call order? In Ansible, role deps run before the role. In bosun, should `role.toml` `requires_role` be a soft constraint (asserts compatibility) or a hard load-and-run-first contract?

6. **Bundle archive content-addressing.** Bundle digest = sha256 of tarball. But tarball is non-deterministic (mtimes, file order, compression). We need a deterministic build mode (sorted file order, fixed mtimes, gzip level pinned). Reference: Bazel + reproducible-build tooling.

7. **Bundle lifecycle hooks.** Helm has pre-install / post-install / pre-upgrade / post-upgrade hooks. Do we need lifecycle ordering across roles within a bundle (e.g. "patroni::pre_install runs after postgres::install")? If yes, declarative DAG; if no, explicit Starlark function call order in main.star.

## 5. Concrete next steps before writing the spec

1. **Decide the secrets-handling story (open question 4).** It cuts across distribution, inventory format, and runtime. Spec is paralyzed without it. Recommend: bundle carries only `vault:` URI references; bosun-client has a pluggable secrets resolver; v1 ships with one impl (HashiCorp Vault or file:// for dev).

2. **Decide single vs per-role Starlark interpreter (open question 1).** Affects performance budget and isolation guarantees. Recommend: single interpreter, role boundary enforced syntactically via `load("@roles/<name>", ...)` namespacing and `_underscore` privates — matches Bazel/Starlark idiom [12].

3. **Decide reproducible-build mode (open question 6).** Without this, every CI rebuild produces a different digest, breaking signature reuse and audit. Recommend: enforce `tar --sort=name --mtime=@<commit-timestamp>` + `gzip -n` at build. Spec mandates it; agent rejects non-deterministic archives at verify (digest mismatch).

4. **Decide tag selection failure mode.** Two options: (a) `tags.require_one_of()` is mandatory and apply fails fast if missing tags; (b) absence of tags is a valid "no environment selected" state that runs only tag-agnostic resources. Recommend (a) — explicit fails over implicit do-the-wrong-thing (Salt's default-merge lesson [15]).

5. **Define a smoke-test fixture bundle.** Write a multi-role fixture (postgres + pgbouncer + patroni, 3 roles minimum) with prod/staging tags and inventory overlays. Use it as the spec's worked example and as the integration-test artifact. The nginx-demo single-role example is insufficient to validate cross-role concerns.

## References

[1] [Helm chart structure docs](https://helm.sh/docs/topics/charts/)
[2] [Helm subcharts and global values](https://helm.sh/docs/chart_template_guide/subcharts_and_globals/)
[3] [Helm Issue 11933 — subcharts same dep different version template clash](https://github.com/helm/helm/issues/11933)
[4] [Ansible tags docs — import_role vs include_role inheritance](https://docs.ansible.com/projects/ansible/latest/playbook_guide/playbooks_tags.html)
[5] [Migrating roles to collections on Galaxy](https://docs.ansible.com/ansible/latest/dev_guide/migrating_roles.html)
[6] [Salt scale tutorial — pillar render bottleneck](https://docs.saltproject.io/en/latest/topics/tutorials/intro_scale.html)
[7] [Chef attribute precedence reference](https://docs.chef.io/attribute_precedence/)
[8] [DNSimple — Tales of a Chef Workflow: Attribute Layering](https://blog.dnsimple.com/2017/04/attribute-layering/)
[9] [Puppet module metadata.json — hard vs soft deps](https://help.puppet.com/core/8/Content/PuppetCore/modules_metadata.htm)
[10] [Terraform dependency lock file scope](https://developer.hashicorp.com/terraform/language/files/dependency-lock)
[11] [Skycfg (Stripe) — Starlark for k8s config](https://github.com/stripe/skycfg)
[12] [Bazel visibility — load visibility, private `_underscore` symbols](https://bazel.build/concepts/visibility)
[13] [Secret management on NixOS with sops-nix](https://michael.stapelberg.ch/posts/2025-08-24-secret-management-with-sops-nix/)
[14] [Pulumi multi-language components](https://www.pulumi.com/blog/pulumiup-pulumi-packages-multi-language-components/)
[15] [Salt pillarenv / saltenv merge behavior issue](https://github.com/saltstack/salt/issues/40848)
[16] [Helm chart signing with cosign — 2025 walkthrough](https://devopsie.com/2025-10-14/goodbye-index-yaml-hello-oci-a-hands-on-helm-guide-to-pushing-and-signing-your-charts.html)
[17] [Helm security: chart signing, repo safety, template hardening](https://hadess.io/helm-security-chart-signing-repository-template/)
[18] [Chef Policyfile vs Berkshelf — current best practice](https://docs.chef.io/policyfile/)
[19] [How to handle secrets in Helm — GitGuardian](https://blog.gitguardian.com/how-to-handle-secrets-in-helm/)
[20] [OCI artifacts explained — beyond container images](https://oneuptime.com/blog/post/2025-12-08-oci-artifacts-explained/view)
[21] [Pulumi component resources architecture](https://www.pulumi.com/docs/iac/concepts/components/)
