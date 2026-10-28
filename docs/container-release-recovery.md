# Native container builds and image-only recovery

The container jobs in `.github/workflows/release.yml` build `linux/amd64` on
`ubuntu-latest` and `linux/arm64` on `ubuntu-24.04-arm`. Neither uses QEMU. Each
architecture has a separate GitHub Actions BuildKit cache scope. A cold build
still compiles its dependencies; native build duration and cache reuse must be
measured in Actions before promising a release time.

On a pushed `v*` tag, each successful build publishes by immutable digest and
uploads its digest as a run artifact. The `image-index` job waits for the entire
matrix to succeed, downloads both artifacts, and validates both SHA-256 strings
before invoking Docker. It creates the version and `latest` tags from those two
references, then inspects both tags. A failed platform cannot publish a partial
index. Registry operations are not transactional: inspection failure after
creation requires checking registry state before attempting recovery.

Every manual `workflow_dispatch` run of `release.yml` is build-only, including
one selecting a version tag. It does not log in to GHCR, push images, publish an index, or create a
GitHub release. Binary build artifacts may still be uploaded to the workflow
run. This corrects the old ref-only guard, which allowed a manual run on a version
tag to publish. Existing workflow token permissions are unchanged.

## Recover the existing v0.1.0 image separately

`.github/workflows/recover-v010-image.yml` is a separate, manual, image-only
recovery entry point. Its application source is fixed to
`f4b7c3794125ae8169ff4ab65c2f378267f5ca5c`; the workflow checks that the existing
`v0.1.0` Git tag still resolves to that commit. The workflow revision and
application source revision are distinct and must both appear in the recovery
record. The `v0.1.0` Git tag and existing GitHub release binary assets stay
unchanged. Re-running the original release run uses its original workflow
revision and does not pick up the native-build fix.

The recovery workflow runs only from `main` in `Torutesu/Shogunagent`. Its
`publish` boolean defaults to `false`. A dry run builds the fixed source on both
native runners, loads each image locally, and runs the CLI's `--help` with
networking disabled. This establishes native executable startup, not a full
memory API or persistence test. Inspect both platform jobs and record their
source revisions, results, build durations, and cache behavior before approving
publication. The separate dry run is an operator prerequisite; the workflow does
not query or enforce a prior run receipt.

A publishing run repeats the local build and network-disabled smoke, then
pushes a second cached build by immutable digest. It pulls that exact digest,
requires its image configuration digest to equal the locally tested image,
and repeats the CLI smoke before recording the platform digest. A mismatch or
failure can leave an untagged registry digest, but cannot advance to version-tag
publication. Provenance/SBOM attestations are disabled here so the final index
has exactly the two runnable platform manifests.

The run uses the existing GitHub Actions package grant; it does not request
repository contents write access or additional Actions permissions. It must establish authenticated GHCR
read access and refuse publication if either `v0.1.0` or `latest` already exists.
Denied or uncertain responses stop the run; they are not evidence that a tag is
absent. Previous manifest inspection was denied. This workflow does not bypass
that denial or provision credentials or permissions.

Both native builds must succeed before the combined index is published. Preserve
the platform digest receipts and final registry evidence. The completed check
must show the same index digest for `v0.1.0` and `latest`, exactly `linux/amd64`
and `linux/arm64`, and the fixed application source revision on both images.
No binaries are rebuilt for GitHub release upload, and no release assets are
created, replaced or deleted by this recovery.

Before dispatching with `publish: true`, a maintainer must review the dry-run
results and authorize the registry writes. Record the pre-existing Git tag
target and release asset checksums, then compare them after recovery. If either
registry tag already exists, inspect the reported metadata and stop: this
workflow deliberately does not overwrite it, even if its digest appears to
match the intended release. A partial publication or an inspection failure
needs a separate recovery decision rather than an automatic retry.

Registry writes are not transactional. Serializing this recovery workflow does
not lock out other publishers; maintainers must avoid concurrent publication
through the normal release workflow or another client. A last-moment preflight
reduces the race but cannot supply registry compare-and-set semantics.

Local tests use fake Docker and registry responses. They verify refusal paths,
command construction, source checks and publication gates without accessing
GHCR. They do not establish real registry access, native build speed, cache
performance, or completed publication.
