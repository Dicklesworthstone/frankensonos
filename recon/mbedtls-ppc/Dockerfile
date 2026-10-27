# Offline PPC32-BE harness for the 57.23 device libmbedtls (bead frankensonos-wbz).
# Runs the device's own libmbedtls.so.14 / libmbedx509.so.1 / libmbedcrypto.so.7 under
# qemu-ppc user mode with the device's own ld.so.1 + libc.so.6 (glibc 2.3.5).
FROM debian:sid

RUN apt-get update && apt-get install -y --no-install-recommends \
        gcc-powerpc-linux-gnu libc6-dev-powerpc-cross qemu-user-static \
        python3 ca-certificates git curl make \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /w
