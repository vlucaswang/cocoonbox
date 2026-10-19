# The Android box base: cocoon's Ubuntu 24.04 VM image with docker, the baked redroid container of bake.sh, scrcpy-rfb,
# the ashmem module for its kernel, the UI server and the desk runtime. A cbdroid template is this base plus the
# agent of a release (image/android/cut.sh). Build context: image/.

# the dex jar is the same for every architecture, so it builds once on the build platform
FROM --platform=$BUILDPLATFORM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac AS ui
RUN apt-get update && apt-get install -y --no-install-recommends openjdk-21-jdk-headless curl ca-certificates && rm -rf /var/lib/apt/lists/*
COPY android/ui /ui
RUN bash /ui/build.sh /cb-ui.jar

FROM ghcr.io/cocoonstack/cocoon/ubuntu:24.04@sha256:54572ca079710227271eb7210e88a48e476d1ce286f2a3ebb733d4162f66e9e2
ARG TARGETARCH
ADD android/base/docker-data-${TARGETARCH}.tar /var/lib/docker/
COPY android/provision.sh /tmp/cb-image/android/provision.sh
COPY android/rootfs /tmp/cb-image/android/rootfs
COPY android/ashmem /tmp/cb-image/android/ashmem
COPY --from=ui /cb-ui.jar /tmp/cb-image/android/ui/cb-ui.jar
COPY android/base/verify.sh /tmp/cb-image/android/base/verify.sh
COPY desk/shuttle.sh /tmp/cb-image/desk/shuttle.sh
COPY desk/rootfs/usr/local/bin/desk /tmp/cb-image/desk/rootfs/usr/local/bin/desk
RUN bash /tmp/cb-image/android/provision.sh && \
    bash /tmp/cb-image/android/base/verify.sh && \
    rm -rf /tmp/cb-image /var/lib/apt/lists/* && \
    printf 'cbdroid base %s\n' "$(date -u +%FT%TZ)" > /etc/cocoonbox/android-base

CMD ["/sbin/init"]
