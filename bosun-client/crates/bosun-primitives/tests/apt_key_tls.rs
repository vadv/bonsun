//! Smoke-тест: `RealAptKeyBackend::download` должен уметь отвечать на
//! HTTPS-URL, а не падать с «no TLS backend configured».
//!
//! Без feature `tls` ureq собирается с заглушкой `NoTlsConfig`, и любой
//! https-URL возвращает ошибку «cannot make HTTPS request because no TLS
//! backend is configured». Это сделало бы apt.key с пакетных зеркал
//! (которые отдают ключи через https) неработоспособным.
//!
//! Этот тест НЕ ходит в реальный интернет. Он бьёт по адресу
//! `https://127.0.0.1:1` (TCP-connect упадёт мгновенно), и проверяет, что
//! ошибка — про connect/transport, а не про отсутствующий TLS backend.

#![allow(clippy::unwrap_used, clippy::panic)]

use bosun_primitives::{AptKeyBackend, RealAptKeyBackend};

#[test]
fn https_download_does_not_panic_with_no_tls_backend_error() {
    let backend = RealAptKeyBackend;
    // Порт 1 заведомо ничего не слушает, плюс tcp-connect к 127.0.0.1
    // отдаст error быстро. Никакого реального HTTPS-handshake не будет —
    // нам важно лишь, что код ureq дошёл до connect-фазы, а не отбился
    // ещё на стадии «нет TLS-бэкенда».
    let err = match backend.download("https://127.0.0.1:1/key.gpg") {
        Err(e) => e,
        Ok(_) => panic!("download to closed port must fail, got Ok"),
    };

    // Конкретный текст ureq при отсутствии feature `tls`: «cannot make
    // HTTPS request because no TLS backend is configured». Это сообщение
    // не должно встретиться — наличие feature `tls` гарантирует, что
    // ureq доходит до фактического connect и возвращает transport-error.
    assert!(
        !err.contains("no TLS backend"),
        "TLS feature is not wired: ureq отдало 'no TLS backend configured' \
         error для https URL. Должно дойти до TCP/TLS handshake. err: {err}",
    );
}
