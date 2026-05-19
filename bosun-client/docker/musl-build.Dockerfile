# Воспроизводимая сборка bosun под x86_64-unknown-linux-musl без
# зависимости от musl-toolchain на хосте. Используется через
# `make musl-docker` или из CI-пайплайна.
#
# rust:alpine построен на musl-libc, поэтому компилятор и линкер
# уже умеют работать с musl-target из коробки. Дополнительный пакет
# musl-dev нужен для `ring` (он собирает свой ASM/C код через `cc`-крейт).
#
# Образ выступает в роли builder'а: сам проект монтируется в /work на
# каждый запуск (см. Makefile), а здесь только тулчейн.
FROM rust:1-alpine

# build-base включает gcc/g++/make; musl-dev нужен для статической линковки
# C-кода из ring; pkgconfig — на всякий случай для transitive-зависимостей.
# Кеш apk удаляем, чтобы образ оставался компактным.
RUN apk add --no-cache build-base musl-dev pkgconfig

# Заранее добавляем оба musl-target, чтобы make musl-docker TARGET=...
# работал без обращения к rustup index в момент запуска.
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

WORKDIR /work
