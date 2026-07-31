# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

ARG BASE_IMAGE
FROM ${BASE_IMAGE}

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        cmake \
        nasm \
        rsync \
    && rm -rf /var/lib/apt/lists/*

COPY build-in-container.sh /usr/local/bin/build-cca-qemu-firmware
RUN chmod 0755 /usr/local/bin/build-cca-qemu-firmware \
    && mkdir -p /work /cache /out \
    && chmod 1777 /work /cache /out

ENTRYPOINT ["/usr/local/bin/build-cca-qemu-firmware"]
