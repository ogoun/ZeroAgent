//! Зонд llama.cpp на поддельном сервере `/props`.

mod support;

use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use za_llm::{ProbeError, probe_llama};

/// Поднимает сервер на 127.0.0.1:0 с одним маршрутом `/props`, отдающим `body`
/// с заданным `content_type`. Возвращает базовый URL.
async fn serve(body: &'static str, content_type: &'static str) -> String {
    let app = Router::new().route(
        "/props",
        get(move || async move { ([(header::CONTENT_TYPE, content_type)], body).into_response() }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn reads_n_ctx_from_root() {
    let base = serve(
        r#"{"n_ctx":32768,"model_path":"C:/models/m.gguf","total_slots":4}"#,
        "application/json",
    )
    .await;
    let p = probe_llama(&base).await.unwrap();
    assert_eq!(p.n_ctx, 32768);
    assert_eq!(p.model, "m.gguf");
    assert_eq!(p.total_slots, Some(4));
}

#[tokio::test]
async fn reads_n_ctx_from_generation_settings() {
    let base = serve(
        r#"{"default_generation_settings":{"n_ctx":8192}}"#,
        "application/json",
    )
    .await;
    // Завершающий слеш в базовом URL не должен ломать путь.
    let p = probe_llama(&format!("{base}/")).await.unwrap();
    assert_eq!(p.n_ctx, 8192);
    assert_eq!(p.model, "");
    assert_eq!(p.total_slots, None);
}

#[tokio::test]
async fn alias_preferred_over_path() {
    let base = serve(
        r#"{"n_ctx":4096,"model_alias":"qwen","model_path":"/m/x.gguf"}"#,
        "application/json",
    )
    .await;
    let p = probe_llama(&base).await.unwrap();
    assert_eq!(p.model, "qwen");
}

#[tokio::test]
async fn html_body_is_bad_response() {
    let base = serve("<html><body>Not llama</body></html>", "text/html").await;
    let err = probe_llama(&base).await.unwrap_err();
    assert!(matches!(err, ProbeError::BadResponse(_)), "{err:?}");
}

#[tokio::test]
async fn missing_n_ctx_is_bad_response() {
    let base = serve(r#"{"model_path":"m.gguf"}"#, "application/json").await;
    let err = probe_llama(&base).await.unwrap_err();
    assert!(matches!(err, ProbeError::BadResponse(_)), "{err:?}");
}

#[tokio::test]
async fn not_found_is_bad_response() {
    let base = serve("{}", "application/json").await;
    // Сервер отвечает 404 на путь вне `/props`: базовый URL с лишним сегментом.
    let err = probe_llama(&format!("{base}/nope")).await.unwrap_err();
    assert!(matches!(err, ProbeError::BadResponse(_)), "{err:?}");
}

#[tokio::test]
async fn connection_refused_is_unreachable() {
    // Занимаем свободный порт и сразу освобождаем: на нём никто не слушает.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let started = std::time::Instant::now();
    let err = probe_llama(&format!("http://127.0.0.1:{port}"))
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::Unreachable(_)), "{err:?}");
    assert!(started.elapsed() < support::scaled(std::time::Duration::from_secs(6)));
}

#[tokio::test]
#[ignore = "нужен запущенный llama-server: LLAMA_TEST_URL"]
async fn live_probe() {
    let url = std::env::var("LLAMA_TEST_URL").expect("LLAMA_TEST_URL");
    let p = probe_llama(&url).await.unwrap();
    assert!(p.n_ctx > 0, "{p:?}");
}
