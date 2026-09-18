FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS build

ARG BUBBLEWRAP_VERSION=0.12.0
ARG BUBBLEWRAP_SHA256=9760d007363e3abba7c747489910f9f82d9fca53ba3bd3282e396fa3c97a3314

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      build-essential \
      ca-certificates \
      curl \
      libcap-dev \
      meson \
      pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL \
      "https://github.com/containers/bubblewrap/releases/download/v${BUBBLEWRAP_VERSION}/bubblewrap-${BUBBLEWRAP_VERSION}.tar.xz" \
      -o /tmp/bubblewrap.tar.xz \
    && echo "${BUBBLEWRAP_SHA256}  /tmp/bubblewrap.tar.xz" | sha256sum --check --strict \
    && mkdir /src \
    && tar --extract --xz --file /tmp/bubblewrap.tar.xz --directory /src --strip-components=1
WORKDIR /src
RUN meson setup _build \
      --buildtype=release \
      --prefix=/media/hud \
      -Dselinux=disabled \
      -Dman=disabled \
      -Dtests=false \
      -Dbash_completion=disabled \
      -Dzsh_completion=disabled \
      -Dbwrapdir=bin \
      -Dinstall_rpath=\$ORIGIN/../lib \
    && meson compile -C _build \
    && meson install -C _build --destdir /out --strip
RUN mkdir -p /out/media/hud/lib /out/media/hud/share/licenses/bubblewrap /out/media/hud/share/licenses/libcap \
    && cp "$(ldd _build/bwrap | awk '/libcap\.so\.2/{print $3; exit}')" \
      /out/media/hud/lib/libcap.so.2 \
    && cp COPYING /out/media/hud/share/licenses/bubblewrap/LICENSE \
    && cp /usr/share/doc/libcap2/copyright /out/media/hud/share/licenses/libcap/copyright \
    && /out/media/hud/bin/bwrap --version

FROM scratch
COPY --from=build /out/media/hud/ /
