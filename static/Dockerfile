# The static half, built and then served by nginx.
#
# This is for the local compose stack (and anywhere else you'd rather serve the
# pages yourself). Production doesn't use it: the pages go to GitHub Pages, which
# is free, and the whole point of the split is to keep bytes off the billed host.
# See README.md.

FROM node:24-alpine@sha256:44fce696f38ba8c987c4db89954051000bbd05ffd8d8659ba15ed4a8602e937f AS builder
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

FROM nginx:alpine@sha256:892bb2152464f0c5d8972593a5f5afaf8054552580b699d95f8ff4af08c2d33d AS runtime
COPY --from=builder /app/dist /usr/share/nginx/html
EXPOSE 80
