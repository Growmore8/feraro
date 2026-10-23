# The terminals.
#
# Vite produces static files; nothing here runs JavaScript at request time.
# The result is a directory of assets that the proxy serves, so this image
# exists only to produce them reproducibly.

FROM node:22-bookworm-slim AS build

WORKDIR /src

# Lockfile first: dependencies are their own layer, and `npm ci` installs
# exactly what the lockfile says rather than what the registry has today.
COPY web/package.json web/package-lock.json ./
RUN npm ci

COPY web/ ./

# The build typechecks before it bundles, so a type error fails the image
# rather than shipping.
RUN npm run build

# ── What ships ──
FROM busybox:1.37-musl
COPY --from=build /src/dist /srv/web
