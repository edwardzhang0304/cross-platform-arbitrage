//! Shared embedded UI resources; no account state or external asset requests.
use axum::{routing::get, Router};

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/assets/openai-market-visuals.js",get(||async{([("content-type","application/javascript; charset=utf-8")],include_str!("../frontend/openai-market-visuals.js"))}))
        .route("/assets/inventory-components.js",get(||async{([("content-type","application/javascript; charset=utf-8")],include_str!("../frontend/inventory-components.js"))}))
        .route("/assets/control-center.css",get(||async{([("content-type","text/css; charset=utf-8")],&include_bytes!("../frontend/control-center.css")[..])}))
        .route("/assets/control-center.js",get(||async{([("content-type","application/javascript; charset=utf-8")],&include_bytes!("../frontend/control-center.js")[..])}))
        .route("/assets/glass-ui.js",get(||async{([("content-type","application/javascript; charset=utf-8")],&include_bytes!("../frontend/glass-ui.js")[..])}))
        .route("/assets/monitor-shell.js",get(||async{([("content-type","application/javascript; charset=utf-8")],&include_bytes!("../frontend/monitor-shell.js")[..])}))
        .route("/assets/monitor.css",get(||async{([("content-type","text/css; charset=utf-8")],&include_bytes!("../frontend/monitor.css")[..])}))
        .route("/assets/phosphor/style.css",get(||async{([("content-type","text/css; charset=utf-8")],&include_bytes!("../frontend/assets/phosphor/style.css")[..])}))
        .route("/assets/phosphor/Phosphor.woff2",get(||async{([("content-type","font/woff2")],&include_bytes!("../frontend/assets/phosphor/Phosphor.woff2")[..])}))
        .route("/assets/brand-glass.png",get(||async{([("content-type","image/png")],&include_bytes!("../frontend/assets/brand-glass.png")[..])}))
        .route("/assets/glass-background.webp",get(||async{([("content-type","image/webp")],&include_bytes!("../frontend/assets/glass-background.webp")[..])}))
}
