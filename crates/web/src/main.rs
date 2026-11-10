#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() {
    use axum::Router;
    use leptos::prelude::*;
    use leptos_axum::{generate_route_list, LeptosRoutes};
    use web::app::{shell, App};

    let conf = get_configuration(None).expect("leptos config");
    let leptos_options = conf.leptos_options;
    let addr = leptos_options.site_addr;
    let routes = generate_route_list(App);

    // Demo mode unless TRIMTAB_ONLINE=1, which also reads live venue funding and 1Click dry quotes.
    let online = std::env::var("TRIMTAB_ONLINE").as_deref() == Ok("1");
    let state = keeper::api::AppState::new(online).expect("keeper state");
    keeper::api::spawn_ticker(state.clone(), std::time::Duration::from_secs(3));

    let ctx_state = state.clone();
    let app = Router::new()
        .leptos_routes_with_context(
            &leptos_options,
            routes,
            move || provide_context(ctx_state.clone()),
            {
                let o = leptos_options.clone();
                move || shell(o.clone())
            },
        )
        .fallback(leptos_axum::file_and_error_handler(shell))
        .with_state(leptos_options)
        .merge(keeper::api::router(state));

    println!("Trimtab on http://{addr}  (DEMO mode: no funds move)");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app.into_make_service()).await.unwrap();
}

#[cfg(not(feature = "ssr"))]
pub fn main() {}
