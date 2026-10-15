# The daemon is cross-compiled outside Docker (cargo zigbuild --target x86_64-unknown-linux-musl), so the image only
# carries the static binary. TLS roots are compiled in (webpki-roots), so no CA bundle is needed.
#
#   cp target/x86_64-unknown-linux-musl/release/fsonos deploy/docker/fsonos
#   docker buildx build --platform linux/amd64 -t fsonos:local deploy/docker
FROM scratch
COPY fsonos /fsonos
ENTRYPOINT ["/fsonos"]
CMD ["serve"]
