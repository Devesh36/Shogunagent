# Deploying ShogunAgent: BYOC, on-prem and regions (E26)

This directory holds what an operator needs to run ShogunAgent in their own infrastructure, and how
we run it ourselves per region:

| Path | What |
|---|---|
| `helm/shogunagent/` | Helm chart: one StatefulSet whose pods hold the gateway and the sidecar, a PVC per replica, Service, NetworkPolicy, ServiceAccount (IRSA / Workload Identity), ConfigMap and Secret wiring |
| `compose/` | `docker-compose.yml` for a single host: sidecar + gateway + Postgres, with `.env.example` and the gateway's `Dockerfile` |
| `check.sh` | The validation CI runs (`.github/workflows/deploy-check.yml`): `helm lint`, `helm template` + kubeconform for the defaults and every `helm/shogunagent/ci/*.yaml`, the settings the chart must refuse, and `docker compose config` |

## 1. What runs where

```
           agents / people (HTTPS)
                    │
          ┌─────────▼──────────────────────── pod (one per replica) ──────────────┐
          │  gateway  (apps/api, Node)   :8787   auth, metering, approvals, relay  │
          │      │ http://127.0.0.1:7464 (loopback only)                           │
          │  sidecar  (shogun-api --multi)       per-principal SQLCipher stores    │
          │      │                                 /data/principals/<p>/memory.db  │
          │      │                                 /data/principals/<p>/<p>.key.wrapped
          └──────┼──────────────────────────────────────────────┬─────────────────┘
                 │ wrap / unwrap data keys                      │ ledger (no bodies)
          customer KMS (AWS KMS, GCP Cloud KMS, Vault Transit)   Postgres
```

- **The sidecar never listens on a network address.** `shogun-api` binds `127.0.0.1` and refuses
  any `Host` that is not loopback, so it is reachable only from the gateway container in the same
  pod (Kubernetes) or the same network namespace (compose: `network_mode: service:gateway`). Its
  health probe therefore runs inside the container: `GET /v1/status` over loopback.
- **Bodies stay in the sidecar** (CLAUDE.md invariants 1 and 2). The gateway's Postgres holds keys
  (hashed), wallets, usage and approval ledgers, never a request or response body.
- **Keys come from the customer's KMS** (§3). Nothing in the image, the chart or the compose file
  holds a principal's key.

## 2. Customer-hosted (BYOC) and our-hosted

| | BYOC / on-prem (this directory) | Our-hosted |
|---|---|---|
| Who runs the gateway and the sidecar | The customer, in their account / data centre | Us, one fleet per region |
| Where principal stores live | The customer's volumes | Our volumes in the tenant's region |
| Whose KMS key wraps the store keys | The customer's (AWS KMS, GCP Cloud KMS, Vault) | Ours, per region; a customer may bring their own key (we then call their KMS with a role they grant) |
| Model provider keys | The customer's own (`OPENAI_API_KEY`, ...) | Ours, behind the relay |
| Billing | None from us, or hosted billing if they opt in | Our wallet / 402 |

### What crosses the boundary

- **Nothing by default.** A BYOC deployment makes no call to any address of ours: the sidecar talks
  to the customer's KMS, the gateway to the customer's Postgres and their model provider. The
  NetworkPolicy (§5.4) makes that the enforced default rather than a promise.
- **If the customer opts into hosted billing** (a contract for the BYOC gateway; the opt-in switch
  is not wired in this release), the gateway sends metering headers and receipts only:
  `Mcp-Method`, `Mcp-Name`, unit and quantity, cost, latency, the agent key id and a receipt id.
  No request or response body, no principal content, no store key, no wrapped key. The destination
  is added to the NetworkPolicy egress explicitly by the operator.
- **Never**: principal stores, wrapped keys, KMS credentials, approval content or traceability rows
  leave the customer's environment.

## 3. Key providers: the customer's KMS

`shogun-api` takes `--key-provider env|aws-kms|gcp-kms|vault` (or `SHOGUN_KEY_PROVIDER`; the flag
wins; default `env`). It applies to the multi-principal server (`--multi`), the hourly tick
(`--dream-tick`) and `--store-relay-credential`.

### 3.1 The envelope model

- Each principal's SQLCipher key is a random 32-byte **data key**. Only its **wrapped** form is
  stored, as `<root>/<principal>/<principal>.key.wrapped` (JSON: provider, key version, context,
  ciphertext; mode 0600; written atomically) next to `<root>/<principal>/memory.db`.
- Opening a store asks the KMS to **unwrap** it. The plaintext key is held only in memory and
  zeroized when dropped; with `SHOGUN_KMS_KEY_CACHE_SECS` > 0 it stays cached that long, so a
  revoked KMS grant takes effect within that window (and as soon as the store is next opened).
- Every wrap is **bound** to `shogun/sqlcipher/v1/<principal>` (AWS encryption context
  `shogun:context`, GCP additional authenticated data, Vault `associated_data`): a wrapped key copied
  to another principal does not unwrap.
- **Rotation re-wraps, it never re-encrypts the database.** When the KMS reports that the blob was
  wrapped by an older key version (GCP `usedPrimary: false`, a Vault `vault:vN` below
  `latest_version`, an AWS key ARN other than the configured key's), the same data key is wrapped
  again under the current version and the file replaced on that open.
- **Failure is closed.** A KMS that is unreachable, refuses the workload, or answers garbage makes
  that principal's store refuse to open (`503 memory_unavailable` naming the KMS state). There is no
  fallback to an environment key or to plaintext. A store whose wrapped-key file is missing is
  refused, never re-keyed (a new key could not open it anyway).
- **Traceability without bodies.** Every KMS call goes through the single HTTP egress
  (`shogun-core` `llm::transport`) and is logged as one line: provider, operation, destination host,
  request size, status, latency. Never a body (which holds key material), a token or a principal id.
- The hosted gateway's credential store (`RemoteTokenStore`) seals connector tokens with the same
  providers (`KmsSealer`: one data key per row, bound to principal and slot).

### 3.2 Variables

Common (sidecar):

| Variable | Meaning |
|---|---|
| `SHOGUN_KEY_PROVIDER` | `env`, `aws-kms`, `gcp-kms` or `vault` (overridden by `--key-provider`) |
| `SHOGUN_KMS_KEY_CACHE_SECS` | Seconds an unwrapped key stays in memory (default 300; 0 = ask the KMS on every open) |
| `SHOGUN_KMS_TIMEOUT_SECS` | Deadline of one KMS round trip (default 10) |
| `SHOGUN_MULTI_ROOT` / `--multi <dir>` | The principals directory (the wrapped keys live in it) |
| `SHOGUN_MULTI_MAX_OPEN` | Open stores kept per process (default 64) |
| `SHOGUN_PRINCIPAL_TOKENS` | `principal:token[:agent_id],...` (secret) |
| `SHOGUN_ADMIN_TOKEN` | The operator's admin bearer for approvals, grants, teams and principal purges (`DELETE /v1/principals/<id>`) (secret) |
| `SHOGUN_APPROVAL_SIGNING_KEY` | Approval token key shared with the gateway, >= 32 characters (secret) |
| `SHOGUN_APPROVAL_BASE_URL` | Where a human approves an L3 request |
| `SHOGUN_API_PORT` | Loopback port (default 7464) |
| `SHOGUN_TOKEN_STORE_DIR` | Local sealed credential files (default `<root>/.tokens`) |

`env` (evaluation only): `SHOGUN_DB_KEY` (64 hex) for every principal, `SHOGUN_DB_KEY_<PRINCIPAL>`
for one.

`aws-kms`:

| Variable | Meaning |
|---|---|
| `SHOGUN_AWS_KMS_KEY_ID` | Key id, key ARN or `alias/...` (required) |
| `SHOGUN_AWS_KMS_REGION` | Region (else `AWS_REGION`, else `AWS_DEFAULT_REGION`) |
| `SHOGUN_AWS_KMS_ENDPOINT` | Optional endpoint (VPC endpoint, FIPS), https |
| `AWS_ROLE_ARN` + `AWS_WEB_IDENTITY_TOKEN_FILE` | IRSA (EKS injects both); `AWS_ROLE_SESSION_NAME` optional; `SHOGUN_AWS_STS_ENDPOINT` overrides `https://sts.<region>.amazonaws.com/` |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` | Static credentials when IRSA is not used (secret) |

IAM: `kms:GenerateDataKey`, `kms:Decrypt`, `kms:Encrypt`, `kms:DescribeKey` on the key, ideally
conditioned on `kms:EncryptionContextKeys` = `shogun:context`. EKS Pod Identity
(`AWS_CONTAINER_CREDENTIALS_FULL_URI`) is not supported yet; use IRSA.

`gcp-kms`:

| Variable | Meaning |
|---|---|
| `SHOGUN_GCP_KMS_KEY` | `projects/<p>/locations/<l>/keyRings/<r>/cryptoKeys/<k>` (required; a symmetric key) |
| `SHOGUN_GCP_KMS_ENDPOINT` | Optional (default `https://cloudkms.googleapis.com`) |
| `SHOGUN_GCP_ACCESS_TOKEN_FILE` | A bearer token file refreshed by your agent, read on every call |
| `SHOGUN_GCP_ACCESS_TOKEN` | A static token (development; secret) |
| (neither) | The metadata server (GKE Workload Identity, GCE), `http://metadata.google.internal` only; exclude it from any `HTTP_PROXY` with `NO_PROXY` |

IAM: `roles/cloudkms.cryptoKeyEncrypterDecrypter` on the key for the workload's service account.

`vault` (Transit):

| Variable | Meaning |
|---|---|
| `VAULT_ADDR` | `https://...` (required) |
| `SHOGUN_VAULT_TRANSIT_KEY` | Transit key name (required; `aes256-gcm96` or `chacha20-poly1305`) |
| `SHOGUN_VAULT_TRANSIT_MOUNT` | Mount path (default `transit`) |
| `VAULT_NAMESPACE` | Enterprise namespace, optional |
| `VAULT_TOKEN` / `VAULT_TOKEN_FILE` | A token, or a Vault Agent sink file read on every call (secret) |
| `SHOGUN_VAULT_K8S_ROLE` | Kubernetes auth role; `SHOGUN_VAULT_K8S_MOUNT` (default `kubernetes`), `SHOGUN_VAULT_K8S_TOKEN_FILE` (default the projected service-account token) |

Policy: `update` on `<mount>/encrypt/<key>` and `<mount>/decrypt/<key>`, `read` on
`<mount>/keys/<key>` (without it rotation is not detected, nothing else breaks).

## 4. Regions

- **One gateway + sidecar fleet per region**, each a separate Helm release (or compose host) with
  its own volumes, Postgres and KMS key in that region. `region` in the chart labels every object
  and is passed to both containers as `SHOGUN_REGION`.
- **No cross-region data path.** A sidecar opens only the principal stores on its own volumes and
  unwraps only with its region's KMS key; a gateway talks only to its own pod's sidecar and its own
  region's Postgres. Nothing replicates stores between regions.
- **A tenant is pinned to one region** when it is created; its principals' stores are created in
  that region and its agent keys resolve only at that region's gateway (one hostname per region,
  e.g. `<region>.api.<domain>`). Moving a tenant is an explicit operator migration (backup in the
  old region, restore in the new one, §6.3), never a runtime path.
- Our-hosted Japan (`ap-northeast-1` / `asia-northeast1`) is one such region; BYOC customers pick
  their own.

## 5. Kubernetes (Helm)

```sh
helm install shogunagent deploy/helm/shogunagent -n shogunagent --create-namespace \
  -f my-values.yaml   # region, keyProvider.*, serviceAccount.annotations, secrets.existingSecret
```

`deploy/helm/shogunagent/ci/*.yaml` are worked examples (AWS + IRSA, GCP + Workload Identity with
two shards and local embeddings, Vault with Kubernetes auth).

### 5.1 Pods and storage

A StatefulSet. Each replica has one PVC (`data`, mounted at `/data`) holding
`principals/<principal>/memory.db` and `principals/<principal>/<principal>.key.wrapped`. The default
is one replica.

### 5.2 Sharding (more than one replica)

A principal lives on exactly one replica: `shard = fnv1a64(principal_id) mod replicaCount` (64-bit
FNV-1a over the id's bytes), served by pod `<release>-<shard>` (stable names through the headless
Service). The chart and the sidecar do not route: a sidecar serves whatever principals are on its
volume, and creates a store for any principal it is asked about. Whatever routes agent traffic
(an ingress with consistent hashing on a principal header, or the control plane that resolves agent
keys) must send a principal's requests to that pod. The chart refuses `replicaCount > 1` until
`sharding.acknowledged=true`. Changing `replicaCount` moves principals: stop traffic, copy each moved
principal's directory (store **and** wrapped key) to its new shard's volume, then resume.

### 5.3 Security

Non-root (uid 10001), read-only root file system (writable `/data` and an `emptyDir` `/tmp` only),
no privilege escalation, all capabilities dropped, `RuntimeDefault` seccomp. The ServiceAccount
carries the IRSA (`eks.amazonaws.com/role-arn`) or Workload Identity
(`iam.gke.io/gcp-service-account`) annotation. Secrets come from `secrets.existingSecret` (one Secret
read by both containers); `secrets.create` exists for evaluation clusters only.

### 5.4 NetworkPolicy

Ingress: the gateway port only, from `networkPolicy.ingressFrom` (default: pods in the namespace).
Egress: DNS, plus the CIDRs in `networkPolicy.egress.llmRelayCidrs` / `kmsCidrs` on
`networkPolicy.egress.ports` and `databaseCidrs` on 5432; the metadata server when the provider is
`gcp-kms`. Standard NetworkPolicy matches addresses, not host names: use your provider's published
ranges or a VPC/private endpoint, or a CNI with FQDN policies through `egress.extraRules`.

### 5.5 Probes and resources

Sidecar: exec liveness/readiness `GET /v1/status` over loopback (the image has no curl; the probe
uses bash). Gateway: HTTP `/healthz`. Requests/limits are in `values.yaml`.

### 5.6 Local embeddings (optional)

`embedLocal.enabled` mounts `modelsVolume` at `/models` (`SHOGUN_MODELS_DIR`) and
`onnxRuntimeVolume` at `/opt/onnxruntime` (`ORT_DYLIB_PATH`), both read-only. Fetch the model once
with `shogun models fetch` into that volume. Without them search stays lexical.

## 6. Operator runbook

### 6.1 Bootstrap

1. Create the KMS key in the tenant's region (AWS: symmetric; GCP: symmetric `ENCRYPT_DECRYPT`;
   Vault: `vault write -f transit/keys/<key>`), and grant the workload identity the permissions in
   §3.2.
2. Create the Secret (principal tokens, admin token, approval signing key, webhook secret, provider
   keys, `RELAY_DATABASE_URL`).
3. Provision the gateway's Postgres schema (`apps/api/DEPLOY.md` §1; compose does it on first start
   from `compose/initdb/`).
4. Install the chart (or `docker compose up -d --build`). The first request for a principal creates
   its directory, its wrapped data key and its store. `kubectl logs` shows one `shogun-kms:` line
   per KMS call and `shogun-api: key provider: <type>` at start.

### 6.2 Key rotation

- **KMS key version rotation** (AWS automatic rotation, GCP rotation schedule, `vault write -f
  transit/keys/<key>/rotate`): nothing to do. AWS keeps decrypting old blobs under the same key
  ARN; GCP and Vault blobs are re-wrapped on each principal's next open. To finish a rotation
  promptly, open every principal (or restart the sidecar and let traffic do it). On Vault, raise
  `min_decryption_version` only after every wrapped file names the new version
  (`grep -h key_version /data/principals/*/*.key.wrapped | sort | uniq -c`).
- **Moving to a different AWS key** (a new key behind the alias, or a new `SHOGUN_AWS_KMS_KEY_ID`):
  keep the old key enabled and the workload allowed to `kms:Decrypt` with it, switch the configured
  key, and let opens re-wrap (AWS `Decrypt` finds the old key from the blob). Disable the old key
  only after no wrapped file names its ARN.
- **Moving to a differently named GCP or Vault key** is not done online in this release: the
  configured key must be the one that wrapped the files. It needs an offline re-wrap tool that does
  not exist yet; until then rotate versions of the same key instead.
- **The data key itself never rotates in place**: that would re-encrypt the database (a SQLCipher
  rekey), which this release does not automate.

### 6.3 Backup and restore

- Back up each principal's **directory**: `memory.db` **and** `<principal>.key.wrapped` together
  (and `memory.db-wal` if present; take the backup with the store closed, or use a volume snapshot,
  which is crash-consistent because SQLite runs in WAL mode). A store without its wrapped key is
  unrecoverable; a wrapped key without the KMS key is too, so KMS key deletion protection (AWS
  pending-deletion window, GCP destroy schedule, Vault `deletion_allowed=false`) is part of the
  backup policy.
- Backups are ciphertext end to end (SQLCipher pages under a key that exists in clear only in the
  sidecar's memory), so they may sit in ordinary object storage in the same region.
- Restore: stop the sidecar (or scale to 0), put the directory back on the right shard's volume,
  start. The wrapped key unwraps with the same KMS key (or any later version of it).

### 6.4 Disaster recovery

- **Volume lost**: restore the directories from the latest backup onto a fresh PVC (same shard).
- **KMS unavailable**: every store whose key is not in the sidecar's cache refuses to open
  (`503 memory_unavailable`); nothing is served from a fallback key. Service resumes when the KMS
  answers. Keep the KMS multi-AZ (it is by default on AWS and GCP; run Vault HA).
- **Region lost**: restore the region's backups into a new fleet in the same region (or, with the
  tenant's written consent, another region, which moves its pin; §4), with a KMS key that can
  unwrap them (a multi-region AWS key replica or a key imported from escrow).

### 6.5 Upgrades and schema migrations

- Migrations are additive and run when a store is opened: the new sidecar migrates each principal's
  store forward on its first open after the upgrade.
- **The sidecar refuses to serve a store whose schema is newer than the binary** (the migration
  history names versions it does not know); the request gets `503 memory_unavailable` and the store
  is left untouched. So a downgrade after stores have migrated is not possible without restoring
  backups taken before the upgrade.
- Procedure: take a backup (§6.3), roll the StatefulSet (`helm upgrade`; one replica at a time),
  watch for `memory_unavailable` in the gateway's responses, and keep the backup until the new
  version has served every active principal.
