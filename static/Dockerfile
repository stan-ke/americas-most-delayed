# The static half, built and then served by nginx.
#
# This is for the local compose stack (and anywhere else you'd rather serve the
# pages yourself). Production doesn't use it: the pages go to GitHub Pages, which
# is free, and the whole point of the split is to keep bytes off the billed host.
# See README.md.

FROM node:24-alpine@sha256:d32cdf619f63fe0471182d08996dd516c6275bb5fd31ae06e55a570bd9e1ad43 AS builder
WORKDIR /app
# Dependencies first, so a source-only change doesn't reinstall them.
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
# The API origin is baked in at build time. Left empty, the page falls back to
# http://localhost:8080 when it's loaded from localhost — which is exactly the
# compose setup — and to the deployed API otherwise. See src/lib/api.ts.
ARG VITE_AMD_API=""
ENV VITE_AMD_API=$VITE_AMD_API
RUN npm run build

FROM nginx:alpine@sha256:4a73073bd557c65b759505da037898b61f1be6cbcc3c2c3aeac22d2a470c1752 AS runtime
COPY --from=builder /app/dist /usr/share/nginx/html
EXPOSE 80
