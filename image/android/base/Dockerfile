# The Android box base: cocoon's Ubuntu 24.04 VM image with docker, the baked redroid container of bake.sh, scrcpy-rfb,
# the ashmem module for its kernel and the desk runtime. A cbdroid template is this base plus the
# agent of a release (image/android/cut.sh). Build context: image/.

FROM ghcr.io/cocoonstack/cocoon/ubuntu:24.04@sha256:54572ca079710227271eb7210e88a48e476d1ce286f2a3ebb733d4162f66e9e2
ARG TARGETARCH
ADD android/base/docker-data-${TARGETARCH}.tar /var/lib/docker/
COPY android/provision.sh /tmp/cb-image/android/provision.sh
COPY android/rootfs /tmp/cb-image/android/rootfs
COPY android/ashmem /tmp/cb-image/android/ashmem
COPY android/base/verify.sh /tmp/cb-image/android/base/verify.sh
COPY desk/shuttle.sh /tmp/cb-image/desk/shuttle.sh
COPY desk/rootfs/usr/local/bin/desk /tmp/cb-image/desk/rootfs/usr/local/bin/desk
RUN bash /tmp/cb-image/android/provision.sh && \
    bash /tmp/cb-image/android/base/verify.sh && \
    rm -rf /tmp/cb-image /var/lib/apt/lists/* && \
    printf 'cbdroid base %s\n' "$(date -u +%FT%TZ)" > /etc/cocoonbox/android-base

CMD ["/sbin/init"]
