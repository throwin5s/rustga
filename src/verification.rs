//! Isolated browser process: clearance stays in memory and never enters logs.
use std::time::{Duration, Instant};
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};

pub fn run() {
    let Some(url) = std::env::args().nth(2).filter(|url| {
        reqwest::Url::parse(url).ok().is_some_and(|u| {
            u.scheme() == "https" && u.host_str() == Some("bulbasaur.poke-black-and-white.net")
        })
    }) else {
        return;
    };
    let event_loop = EventLoop::new();
    let Ok(window) = WindowBuilder::new()
        .with_title("Complete MangaBall verification — close to cancel")
        .build(&event_loop)
    else {
        return;
    };
    let Ok(webview) = wry::WebViewBuilder::new()
        .with_incognito(true)
        .with_user_agent(rustga::http::DEFAULT_USER_AGENT)
        .with_url(&url)
        .with_navigation_handler(|url| {
            reqwest::Url::parse(&url).ok().is_some_and(|u| {
                u.scheme() == "https"
                    && (u.host_str() == Some("bulbasaur.poke-black-and-white.net")
                        || u.host_str() == Some("challenges.cloudflare.com"))
            })
        })
        .build(&window)
    else {
        return;
    };
    let started = Instant::now();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(500));
        if matches!(
            event,
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
        ) || started.elapsed() > Duration::from_secs(180)
        {
            *control_flow = ControlFlow::Exit;
        }
        if let Ok(cookies) = webview.cookies_for_url(&url) {
            if let Some(cookie) = cookies
                .iter()
                .find(|cookie| cookie.name() == "cf_clearance")
            {
                use std::io::Write;
                let _ = std::io::stdout()
                    .write_all(format!("cf_clearance={}", cookie.value()).as_bytes());
                *control_flow = ControlFlow::Exit;
            }
        }
    });
}
