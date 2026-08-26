# Wraps the artifacts CI already built. Nothing is compiled here, so the image
# build stays a few seconds rather than repeating the rust-gpu shader compile.
#
# The `cc` distroless variant is the one built for dynamically linked Rust: it
# carries libgcc_s.so.1, which the binary needs for unwinding and which thinner
# glibc bases (busybox:stable-glibc) leave out. Its glibc is 2.41, newer than the
# 2.39 the ubuntu-24.04-arm runner builds against, which is the safe direction.
#
# There is no shell in here. See deploy/README.md for how to get one when
# debugging on the host.
FROM gcr.io/distroless/cc-debian13

WORKDIR /work

COPY target/release/slash0-server /work/slash0-server
COPY crates/client/dist /work/dist

ENTRYPOINT ["/work/slash0-server", "--config", "/work/server.yaml"]
