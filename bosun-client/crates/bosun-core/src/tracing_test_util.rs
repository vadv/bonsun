//! Утилиты для тестирования tracing-событий поверх примитивов и
//! Orchestrator'а. Не входит в production-API, поэтому собирается только
//! под `#[cfg(test)]`.
//!
//! ## Зачем «глобальный роутер»
//!
//! Tracing хранит interest каждого callsite'а в глобальном кэше. Если
//! параллельные тесты по-разному отвечают на `register_callsite` (или
//! сначала бьёт по callsite'у тест без subscriber'а), кэш может застрять
//! в `Interest::never`, и наш recorder увидит пустую очередь, даже когда
//! `enabled` возвращает true.
//!
//! Решение — один раз на процесс установить global default subscriber,
//! который:
//! 1. Всегда говорит `Interest::sometimes` в `register_callsite`, чтобы
//!    callsite ходил в per-event `enabled` через текущий dispatcher.
//! 2. В `event` проверяет thread-local recorder и форвардит запись туда.
//!
//! Тесты вызывают `record_events(|| ...)`: на время замыкания thread-local
//! устанавливается recorder, после выхода — снимается. Это позволяет
//! параллельным тестам безопасно собирать события каждый в свой буфер.

#![allow(clippy::unwrap_used)]

use std::cell::RefCell;
use std::sync::{Arc, Mutex, Once};

use tracing::field::Visit;
use tracing::subscriber::Interest;
use tracing::{span, Event, Metadata, Subscriber};

thread_local! {
    /// Активный recorder текущего треда. Router-subscriber форвардит сюда
    /// события. Снимается после возврата из `record_events`.
    static THREAD_RECORDER: RefCell<Option<Arc<Recorder>>> = const { RefCell::new(None) };
}

static INSTALL: Once = Once::new();

/// Установить global default subscriber. Идемпотентно: повторные вызовы
/// игнорируются (set_global_default разрешает только один). Должно
/// вызываться в каждом тесте, который собирает события, до выполнения
/// проверяемого кода.
pub fn install_global_router() {
    INSTALL.call_once(|| {
        let router = Router;
        // Если другой код уже выставил global default — ничего не делаем.
        // Это маловероятно в #[cfg(test)] контексте, но защищаемся.
        let _ = tracing::subscriber::set_global_default(router);
    });
}

/// Запустить `f` под активным per-thread recorder'ом и вернуть собранные
/// события. Использует Arc внутри thread-local, чтобы recorder пережил
/// возможные `with_default`-вложения.
pub fn record_events<F: FnOnce()>(f: F) -> Vec<String> {
    let recorder = Arc::new(Recorder::new());
    THREAD_RECORDER.with(|cell| {
        *cell.borrow_mut() = Some(Arc::clone(&recorder));
    });
    f();
    THREAD_RECORDER.with(|cell| {
        *cell.borrow_mut() = None;
    });
    recorder.events()
}

/// Внутренний recorder, расшариваемый через Arc между thread-local'ом
/// и возвращаемой коллекцией событий.
pub(crate) struct Recorder {
    events: Mutex<Vec<String>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }
    fn push(&self, message: String) {
        self.events.lock().unwrap().push(message);
    }
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

struct MessageVisitor<'a> {
    out: &'a mut String,
}
impl<'a> Visit for MessageVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.out = format!("{value:?}");
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            *self.out = value.to_string();
        }
    }
}

/// Global subscriber. Не сохраняет события сам — форвардит активному
/// per-thread recorder'у. Если thread-local пустой, событие просто
/// игнорируется, как обычный no-op subscriber.
struct Router;

impl Subscriber for Router {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        // Sometimes форсит per-event `enabled` — это позволяет per-thread
        // recorder'у решать судьбу события на ходу.
        Interest::sometimes()
    }
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        // Всегда true: фильтрация на стороне per-thread (если recorder
        // не установлен, событие просто не сохранится).
        true
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        // Span-IDs не нужны для message-recording: возвращаем фиксированный.
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut message = String::new();
        event.record(&mut MessageVisitor { out: &mut message });
        if message.is_empty() {
            return;
        }
        THREAD_RECORDER.with(|cell| {
            if let Some(recorder) = cell.borrow().as_ref() {
                recorder.push(message);
            }
        });
    }
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}
