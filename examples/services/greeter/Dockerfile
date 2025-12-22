# Build stage
FROM node:22-alpine AS builder
WORKDIR /app
COPY package*.json ./
RUN apk add --no-cache zip
RUN npm ci
COPY tsconfig.json ./
COPY src ./src
RUN npm run bundle

# Production stage
FROM node:22-alpine
RUN apk add --no-cache dumb-init

WORKDIR /app
COPY --from=builder /app/package*.json ./
RUN npm ci --omit=dev
COPY --from=builder /app/dist ./dist

ENV PORT=9080
EXPOSE 9080

CMD ["dumb-init", "node", "dist/app.js"]