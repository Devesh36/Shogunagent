# The gateway (apps/api) as a container, for BYOC: the Node entry (src/index.ts) that production
# runs on Workers in our-hosted mode. Build from the repository root:
#
#   docker build -f deploy/compose/gateway.Dockerfile -t shogunagent-gateway:0.1.0 .
#
# It never reads or stores request or response bodies (CLAUDE.md invariant 1/2); every setting is
# documented in apps/api/src/boot.ts and apps/api/src/index.ts.

FROM node:22-bookworm-slim
WORKDIR /app
RUN corepack enable
# The workspace (the .dockerignore keeps target/, node_modules/, .git out). apps/api imports
# packages/config/license-public-key.json by path.
COPY . .
RUN pnpm install --frozen-lockfile --filter "@shogun-ai/api..." \
 && chown -R node:node /app/apps/api
USER node
WORKDIR /app/apps/api
ENV NODE_ENV=production PORT=8787
EXPOSE 8787
# tsx compiles in memory; its cache goes to /tmp (a tmpfs / emptyDir under a read-only root).
CMD ["./node_modules/.bin/tsx", "src/index.ts"]
