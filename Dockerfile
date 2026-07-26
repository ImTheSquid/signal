# One image, three processes: the SvelteKit server and the two websocket
# servers. They share the workspace deps, so building them separately would
# just mean installing node_modules three times.
FROM node:22-alpine AS build
RUN npm i -g pnpm@11
WORKDIR /app

COPY pnpm-workspace.yaml pnpm-lock.yaml package.json ./
COPY packages/protocol/package.json packages/protocol/
COPY packages/validator-wasm/package.json packages/validator-wasm/
COPY apps/web/package.json apps/web/.npmrc apps/web/
RUN pnpm install --frozen-lockfile

COPY packages/ packages/
COPY apps/web/ apps/web/
RUN pnpm -r build

FROM node:22-alpine AS runtime
RUN npm i -g pnpm@11
WORKDIR /app
ENV NODE_ENV=production

# tsx stays available: the websocket servers run from TypeScript source.
COPY --from=build /app/ ./

WORKDIR /app/apps/web
EXPOSE 3000 3001 3002
CMD ["node", "build/index.js"]
