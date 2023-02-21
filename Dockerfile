FROM rust:alpine as build

ARG STACKS_NODE_VERSION="No Version Info"
ARG GIT_BRANCH='No Branch Info'
ARG GIT_COMMIT='No Commit Info'

WORKDIR /src

COPY . .

RUN apk add --no-cache musl-dev openssl-dev

RUN mkdir /out /contracts

RUN cd testnet/stacks-node && cargo build --features monitoring_prom,slog_json --release

RUN cp target/release/subnet-node /out

FROM alpine:latest

RUN apk add --no-cache openssl musl

COPY --from=build /out/ /bin/
# Add the core contracts to the image, so that clarinet can retrieve them.
COPY --from=build /src/core-contracts/contracts/subnet.clar /contracts/subnet.clar
COPY --from=build /src/core-contracts/contracts/helper/subnet-traits.clar /contracts/subnet-traits.clar

CMD ["subnet-node", "start"]
