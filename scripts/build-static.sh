#!/bin/sh

podman run --rm -it -w /data -v /home/akitaki/code/telegram-sticker-search-bot:/data --env CARGO_TARGET_DIR=./static-target rust:alpine /bin/sh -c 'apk add musl-dev; cargo build --release'
