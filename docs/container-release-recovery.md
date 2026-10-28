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

Every manual `workflow_dispatch` run is build-only, including one selecting a
version tag. It does not log in to GHCR, push images, publish an index, or create a
GitHub release. Binary build artifacts may still be uploaded to the workflow
run. This corrects the old ref-only guard, which allowed a manual run on a version
tag to publish. Existing workflow token permissions are unchanged.

## Recover the existing v0.1.0 image separately

This is a proposed recovery procedure, not a publication performed by this
change. The existing release source is
`f4b7c3794125ae8169ff4ab65c2f378267f5ca5c`. Keep the `v0.1.0` Git tag and existing
GitHub release binary assets unchanged. Re-running the original release run
uses its original workflow revision, so it does not pick up this native-build
fix. A manual run of the updated workflow intentionally cannot publish.

1. Review and merge the native-build fix. Run the updated workflow manually on
   its reviewed commit to validate native runner availability, Docker builds,
   cache behavior and build duration. Record the source SHA and run ID. This
   dry run validates that commit; it is not evidence that the older release
   source was rebuilt.
2. Prepare a separately reviewed **image-only** recovery workflow or operator
   procedure that checks out the full source SHA above on both native runners.
   The workflow revision and application source revision must be recorded
   separately. It must use that source tree's Dockerfile and build context,
   preserve the per-architecture cache separation, and label the image revision
   with the application source SHA. Do not create a replacement Git tag, rerun
   binary publication, or upload/delete release assets.
3. First build both platforms without pushing. Record successful builds and
   platform identity. Before any registry write, obtain explicit recovery
   authorization and verify normal GHCR access. Previous manifest inspection
   was denied; that is unresolved access evidence, not proof that a tag is
   absent. Stop if access is still denied; do not change credentials or bypass
   permissions as part of this procedure.
4. After approval, publish both images by digest and retain the two digest
   receipts. Validate both references before creating the `v0.1.0` index. Inspect
   any existing version tag first: do not overwrite an unexpected existing
   digest without a separate maintainer decision. Decide explicitly whether
   `latest` should also point to this release; recovery must not implicitly
   replace a newer release's `latest` tag.
5. Inspect the published index and confirm both `linux/amd64` and `linux/arm64`
   manifests and their revision labels. Record the index digest and run/source
   revisions, then perform image startup checks by digest on each architecture.
   Compare the Git tag target and existing release asset checksums with the
   pre-recovery record to confirm they were untouched.

The image-only recovery entry point is not implemented here. No tag movement,
registry publication, workflow rerun or release-asset replacement is needed to
review this change. Local fake-Docker tests check command construction, digest
validation and publication gates; they do not establish registry access, actual
image architecture, build speed or container runtime correctness.
