use std::{cell::RefCell, rc::Rc};

use bag_lib::{action::Action, ui::LayoutOrAction};
use futures::{
    StreamExt,
    channel::oneshot::{Receiver, Sender},
    select,
};
use leptos::{prelude::*, task::spawn_local};
use web_sys::wasm_bindgen::prelude::*;

use crate::util::FetchError;

mod panel;
mod util;

#[derive(Clone)]
struct Context {
    targets: RwSignal<RenderTargetSet>,
    backend: String,
}

impl Context {
    pub fn new(backend: String, init: String) -> Self {
        let (retire_tx, retire_rx) = futures::channel::oneshot::channel();
        let cur = RenderTarget::new(&backend, init, Some(retire_tx));
        let ret = Self {
            targets: RwSignal::new(RenderTargetSet {
                current: cur,
                next: None,
                prev: None,
            }),
            backend,
        };
        ret.subscribe(&ret.targets.read_untracked().current, retire_rx);
        ret
    }

    pub async fn handle(&self, action: &Action) {
        match action {
            Action::Navigate { to: p } => {
                self.navigate(p.clone());
            }
        }
    }

    // TODO: relative navigation
    pub fn navigate(&self, p: String) {
        let targets = self.targets.read_untracked();
        if p == targets.current.path {
            return;
        }

        web_sys::window()
            .unwrap()
            .history()
            .unwrap()
            .push_state_with_url(&JsValue::NULL, "", Some(&format!("/{}", p)))
            .unwrap();

        #[derive(Debug)]
        enum Operation {
            RotR,
            RotL,
            Reset,
        }

        let op = if let Some(ref prev) = targets.prev
            && prev.path == p
        {
            Operation::RotR
        } else if let Some(ref next) = targets.next
            && next.path == p
        {
            Operation::RotL
        } else {
            Operation::Reset
        };

        drop(targets);

        web_sys::console::log_1(&format!("Navigating to {p} with operation {:?}", op).into());
        let Some(mut targets) = self.targets.try_write() else {
            return;
        };
        web_sys::console::log_1(&format!("Navigating to {p} with operation {:?}", op).into());

        match op {
            Operation::RotR => {
                let replacing = if let Some(prev) = targets.prev.take() {
                    prev
                } else {
                    self.initiate(p)
                };
                let replaced = std::mem::replace(&mut targets.current, replacing);
                targets.next = Some(replaced);
            }
            Operation::RotL => {
                let replacing = if let Some(next) = targets.next.take() {
                    next
                } else {
                    self.initiate(p)
                };
                let replaced = std::mem::replace(&mut targets.current, replacing);
                targets.prev = Some(replaced);
            }
            Operation::Reset => {
                targets.current = self.initiate(p);
                targets.prev = None;
                targets.next = None;
            }
        }

        if let Some(Some(Ok(data))) = targets.current.data.try_get_untracked() {
            drop(targets);
            self.activate(data);
        }
    }

    fn initiate(&self, path: String) -> RenderTarget {
        let (retire_tx, retire_rx) = futures::channel::oneshot::channel();
        let target = RenderTarget::new(&self.backend, path, Some(retire_tx));
        self.subscribe(&target, retire_rx);
        target
    }

    fn activate(&self, data: LayoutOrAction) {
        let Some(mut wr) = self.targets.try_write() else {
            return;
        };
        match data {
            LayoutOrAction::Layout(layout) => {
                let prev_update = layout.left.as_ref() != wr.prev.as_ref().map(|t| &t.path);
                let next_update = layout.right.as_ref() != wr.next.as_ref().map(|t| &t.path);
                if prev_update || next_update {
                    if prev_update {
                        wr.prev = layout.left.map(|p| self.initiate(p));
                    }
                    if next_update {
                        wr.next = layout.right.map(|p| self.initiate(p));
                    }
                } else {
                    wr.untrack();
                }
            }
            LayoutOrAction::Action(action) => {
                let ctx = self.clone();
                spawn_local(async move { ctx.handle(&action).await });
            }
        }
    }

    fn subscribe(&self, target: &RenderTarget, mut retire: Receiver<()>) {
        let data = target.data.clone();
        let path = target.path.clone();
        let targets = self.targets.clone();
        let ctx = self.clone();
        spawn_local(async move {
            let mut stream = data.to_stream().fuse();
            loop {
                select! {
                    _ = retire => {
                        web_sys::console::log_1(&"Retiring render target: {path}".into());
                        break;
                    },
                    update = stream.next() => {
                        if let Some(Some(Ok(update))) = update {
                            web_sys::console::log_1(&format!("Render target updated {path}: {:?}", update).into());

                            if let Some(rd) = targets.try_read_untracked() {
                                // Ptr Eq here
                                if rd.current.data == data {
                                    drop(rd);
                                    ctx.activate(update);
                                }
                            }
                        }
                    }
                }
            }
        });
    }
}

struct RenderTarget {
    path: String,
    // None = fetching
    data: ArcRwSignal<Option<Result<LayoutOrAction, FetchError>>>,
    // Whether this render target is retired on drop.
    // Avoids memory leaks
    retire: Option<Sender<()>>,
}

impl RenderTarget {
    pub fn new(backend: &str, path: String, retire: Option<Sender<()>>) -> Self {
        let url = format!("{backend}/render/{path}");
        let data = ArcRwSignal::new(None);
        {
            let data = data.clone();
            spawn_local(async move {
                let fetched: Result<LayoutOrAction, _> = crate::util::fetch(&url).await;
                web_sys::console::log_1(&format!("Fetched layout: {data:?}").into());
                data.set(Some(fetched));
            })
        }

        Self { path, data, retire }
    }

    pub fn weak(&self) -> RenderTarget {
        RenderTarget {
            path: self.path.clone(),
            data: self.data.clone(),
            retire: None,
        }
    }
}

impl Drop for RenderTarget {
    fn drop(&mut self) {
        web_sys::console::log_2(
            &"Dropping render target for path: {}".to_owned().into(),
            &self.path.clone().into(),
        );
        if let Some(retire) = self.retire.take() {
            retire.send(()).unwrap();
        }
    }
}

struct RenderTargetSet {
    current: RenderTarget,
    next: Option<RenderTarget>,
    prev: Option<RenderTarget>,
}

#[derive(Eq, PartialEq, Hash, Clone, Copy)]
enum RenderTargetPersona {
    Current,
    Next,
    Prev,
}

impl RenderTargetPersona {
    pub fn as_str(&self) -> &'static str {
        match self {
            RenderTargetPersona::Current => "current",
            RenderTargetPersona::Next => "next",
            RenderTargetPersona::Prev => "prev",
        }
    }
}

impl RenderTargetSet {
    pub fn as_targets(&self) -> Vec<RenderTarget> {
        let mut targets = vec![];
        if let Some(ref prev) = self.prev {
            targets.push(prev.weak());
        }
        targets.push(self.current.weak());
        if let Some(ref next) = self.next {
            targets.push(next.weak());
        }
        targets
    }

    pub fn get_persona(&self, path: &str) -> Option<RenderTargetPersona> {
        if self.current.path == path {
            Some(RenderTargetPersona::Current)
        } else if let Some(ref prev) = self.prev
            && prev.path == path
        {
            Some(RenderTargetPersona::Prev)
        } else if let Some(ref next) = self.next
            && next.path == path
        {
            Some(RenderTargetPersona::Next)
        } else {
            None
        }
    }
}

const TOUCH_THRESHOLD: i32 = 10; // px

// Swipe animation tunables:
// - STIFFNESS: how hard the spring pulls the offset back to rest. Larger
//   => snappier return. Sets the natural frequency w = sqrt(STIFFNESS).
// - DAMPING: how aggressively motion bleeds off. The system is required to be
//   *critically* damped (the fastest return that never oscillates/overshoots),
//   so DAMPING is not free: it must satisfy DAMPING^2 == 4 * STIFFNESS. Pick a
//   STIFFNESS for the feel you want, then set DAMPING = 2 * sqrt(STIFFNESS).
//
// There is no separate velocity cap: the inward speed beyond which the
// critically damped solution would cross the resting point (and overshoot) is
// w * |offset|, which depends on the release offset and is computed on release.
//
// Time is measured in real units (milliseconds, matching `dt` in the frame
// loop and the touch-derived velocity), so STIFFNESS is in 1/ms^2, DAMPING in
// 1/ms, and the natural frequency w = sqrt(STIFFNESS) in 1/ms.
const STIFFNESS: f64 = 0.000256;
const DAMPING: f64 = 0.032;

// Enforce critical damping at compile time. Both the return animation and the
// `swipe_decide` projection assume DAMPING^2 == 4 * STIFFNESS; an under- or
// over-damped choice would invalidate the closed-form turning point below.
const _: () = {
    let diff = DAMPING * DAMPING - 4.0 * STIFFNESS;
    assert!(
        diff > -1e-9 && diff < 1e-9,
        "swipe spring must be critically damped: set DAMPING = 2 * sqrt(STIFFNESS)",
    );
};

enum SwipeDecision {
    Keep,
    Right,
    Left,
}

/**
 * Decide navigation at user's touch release
 *
 * threshold is 1/2 viewport width, not full viewport width.
 * It's in the same unit as offset and velocity
 */
fn swipe_decide(offset: f64, velocity: f64, threshold: f64) -> SwipeDecision {
    web_sys::console::log_1(
        &format!(
            "Swipe decision: offset = {}, velocity = {}, threshold = {}",
            offset, velocity, threshold
        )
        .into(),
    );
    // The release animation is a *critically damped* spring returning to rest
    // (offset = 0). With critical damping (DAMPING^2 == 4 * STIFFNESS, asserted
    // above) and natural frequency w = sqrt(STIFFNESS), the trajectory has the
    // closed form
    //     x(t) = (offset + (velocity + w * offset) * t) * exp(-w * t)
    // whose first stationary point (x'(t) = 0, i.e. the furthest the panel
    // coasts before momentum runs out) sits at
    //     t* = velocity / (w * (velocity + w * offset)).
    // We evaluate x(t*) and compare that projected peak against the threshold.
    //
    // Unlike the lossless energy projection this is *asymmetric*: damping bleeds
    // energy while crossing the spring, so from a non-zero offset a flick that
    // reverses past the resting point needs more momentum than one continuing in
    // the current direction -- which matches user intuition.
    let w = STIFFNESS.sqrt();
    let denom = w * (velocity + w * offset);

    const EPS: f64 = 1e-9;
    let peak = if denom.abs() < EPS {
        // velocity + w*offset == 0: the linear term vanishes, the panel decays
        // monotonically to rest and never travels beyond its release offset.
        offset
    } else {
        let t_star = velocity / denom;
        if t_star <= 0.0 {
            // The stationary point is in the past: from here on the panel moves
            // monotonically back to rest, so its furthest excursion is `offset`.
            offset
        } else {
            (offset + (velocity + w * offset) * t_star) * (-w * t_star).exp()
        }
    };

    // Layout is `prev | current | next`, so a positive (rightward) peak past
    // the threshold lands on the next panel, a negative one on the previous.
    if peak > threshold {
        web_sys::console::log_1(&format!("Swipe decision: Right (peak = {})", peak).into());
        SwipeDecision::Right
    } else if peak < -threshold {
        web_sys::console::log_1(&format!("Swipe decision: Left (peak = {})", peak).into());
        SwipeDecision::Left
    } else {
        web_sys::console::log_1(&format!("Swipe decision: Keep (peak = {})", peak).into());
        SwipeDecision::Keep
    }
}

#[component]
fn App() -> impl IntoView {
    console_error_panic_hook::set_once();
    web_sys::console::log_1(&"App mounted".into());
    let local_storage = web_sys::window().unwrap().local_storage().unwrap().unwrap();
    let backend = local_storage.get_item("backend").unwrap().unwrap();
    web_sys::console::log_1(&format!("Using backend: {}", backend).into());
    // FIXME: dynamic backend

    let initial_path = web_sys::window()
        .unwrap()
        .location()
        .pathname()
        .map(|e| e.trim_start_matches('/').trim_end_matches("/").to_owned())
        .unwrap_or_else(|_| String::new());

    let ctx = Context::new(backend, initial_path);
    provide_context(ctx.clone());

    // Listen for popstate events to handle browser navigation (back/forward)
    {
        let ctx = ctx.clone();
        let closure = Closure::wrap(Box::new(move |_event: web_sys::PopStateEvent| {
            let new_path = web_sys::window()
                .unwrap()
                .location()
                .pathname()
                .map(|e| e.trim_start_matches('/').trim_end_matches("/").to_owned())
                .unwrap_or_else(|_| String::new());
            ctx.navigate(new_path);
        }) as Box<dyn FnMut(_)>);
        web_sys::window()
            .unwrap()
            .set_onpopstate(Some(closure.as_ref().unchecked_ref()));
        closure.forget(); // Prevent the closure from being dropped
    }

    #[derive(Clone)]
    enum SwipeState {
        Released {
            release_time: f64,
            release_velocity: f64,
            release_offset: i32,
        },
        Starting {
            init_x: i32,
        },
        Moving {
            init_x: i32,
            last_x: i32,
            last_time: f64,
        },
    }

    impl Default for SwipeState {
        fn default() -> Self {
            SwipeState::Released {
                release_time: 0.0,
                release_velocity: 0.0,
                release_offset: 0,
            }
        }
    }

    // Offset relative to the resting position (current panel at exactly the viewport)
    let offset = RwSignal::new(0f64);
    // User interaction state
    let touching: RwSignal<SwipeState> = RwSignal::new(SwipeState::default());

    // Animation frame loop
    let frame_handler: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let frame_handler_clone = frame_handler.clone();
    *frame_handler.borrow_mut() = Some(Closure::new(move || {
        util::request_animation_frame(frame_handler_clone.borrow().as_ref().unwrap());

        let SwipeState::Released {
            release_time,
            release_velocity,
            release_offset,
        } = touching.get_untracked()
        else {
            // Still touching, don't update offset
            return;
        };

        let now_t = web_sys::window().unwrap().performance().unwrap().now();
        let dt = now_t - release_time;

        const EPS: f64 = 0.2;

        // Closed-form evolution of the *critically damped* spring that returns
        // the panel to its resting state (offset = 0). Because critical damping
        // is enforced (DAMPING^2 == 4 * STIFFNESS), the trajectory has an exact
        // analytic solution, so we evaluate it directly from the time elapsed
        // since release (`dt`, ms) rather than integrating. This is stateless
        // and unconditionally stable: a long lag frame just lands further along
        // the same curve and can never overshoot or blow up.
        //
        // With natural frequency w = sqrt(STIFFNESS) and b = v0 + w * x0:
        //     offset(dt) = (x0 + b * dt) * exp(-w * dt)
        let w = STIFFNESS.sqrt();
        let x0 = release_offset as f64;
        let b = release_velocity + w * x0;
        let mut offset_now = (x0 + b * dt) * (-w * dt).exp();

        // No overshoot: a critically damped trajectory crosses the resting
        // point at most once; once `dt` is past that crossing, hold it at rest.
        if x0 != 0.0 && x0 * offset_now < 0.0 {
            offset_now = 0.0;
        }

        if offset_now.abs() < EPS {
            offset_now = 0.0;
        }
        if offset.get_untracked() != offset_now {
            offset.set(offset_now);
        }
    }));
    util::request_animation_frame(frame_handler.borrow().as_ref().unwrap());

    view! {
        <div class="swipe-root"
            on:pointerdown=move |ev| {
                if ev.pointer_type() != "touch" { return; }
                touching.set(SwipeState::Starting { init_x: ev.client_x() });
            }
            on:pointermove=move |ev| {
                if ev.pointer_type() != "touch" { return; }
                let x = ev.client_x();
                let now = web_sys::window().unwrap().performance().unwrap().now();
                // FIXME: cap offset based on available panels
                match touching.get_untracked() {
                    SwipeState::Released { .. } => { /* ???  */ },
                    SwipeState::Starting { init_x } => {
                        if (x - init_x).abs() < TOUCH_THRESHOLD {
                            return;
                        }
                        touching.set(SwipeState::Moving { init_x, last_x: x, last_time: now });
                        offset.set((x - init_x) as f64);
                    },
                    SwipeState::Moving { init_x, .. } => {
                        ev.prevent_default();
                        offset.set((x - init_x) as f64);
                        // TODO: PID update?
                        touching.set(SwipeState::Moving { init_x, last_x: x, last_time: now });
                    }
                }
            }
            on:pointerup=move |ev| {
                if ev.pointer_type() != "touch" { return; }
                let x = ev.client_x();
                let now = web_sys::window().unwrap().performance().unwrap().now();
                let state = match touching.get_untracked() {
                    s @ SwipeState::Released { .. } => s,
                    SwipeState::Starting { .. } => SwipeState::Released {
                        release_time: now,
                        release_velocity: 0f64,
                        release_offset: offset.get_untracked().round() as i32,
                    },
                    SwipeState::Moving { init_x, last_x, last_time } => {
                        let mut offset_now = (x - init_x) as f64;
                        let velocity_now = (x - last_x) as f64 / (now - last_time);
                        let vw = web_sys::window().unwrap().inner_width().unwrap().as_f64().unwrap();
                        match swipe_decide(offset_now, velocity_now, vw / 2.0) {
                            SwipeDecision::Keep => {},
                            SwipeDecision::Right => {
                                let targets = ctx.targets.read_untracked();
                                if let Some(prev) = targets.prev.as_ref() {
                                    let path = prev.path.clone();
                                    drop(targets);
                                    ctx.navigate(path);
                                    offset_now -= vw;
                                }
                            },
                            SwipeDecision::Left => {
                                let targets = ctx.targets.read_untracked();
                                if let Some(next) = targets.next.as_ref() {
                                    let path = next.path.clone();
                                    drop(targets);
                                    ctx.navigate(path);
                                    offset_now += vw;
                                }
                            },
                        }
                        offset.set(offset_now);
                        // Cap only the *inward* release speed. The critically
                        // damped solution stays monotonic (no zero crossing, no
                        // overshoot) iff the inward speed is at most w * |x0|,
                        // where w = sqrt(STIFFNESS); at that limit b = 0 and the
                        // panel decays purely exponentially home. Outward motion
                        // never overshoots, so it is left untouched.
                        let w = STIFFNESS.sqrt();
                        let limit = w * offset_now.abs();
                        let release_velocity = if offset_now > 0.0 {
                            velocity_now.max(-limit)
                        } else if offset_now < 0.0 {
                            velocity_now.min(limit)
                        } else {
                            velocity_now
                        };
                        SwipeState::Released {
                            release_time: now,
                            release_velocity,
                            release_offset: offset_now as i32,
                        }
                    }
                };
                touching.set(state);
            }
            on:pointercancel=move |ev| {
                if ev.pointer_type() != "touch" { return; }
                touching.set(SwipeState::default());
                offset.set(0.0);
            }
            style:--swipe-offset={move || format!("{}px", offset.get())}
        >
            <For
                each=move || ctx.targets.read().as_targets()
                key=|target| target.path.clone()
                let (target)
            >
                <div class={move || format!("panel panel-{}", ctx.targets.read().get_persona(&target.path).map(|p| p.as_str()).unwrap_or("unknown"))}>
                    <panel::Panel data={target.data.read_only()} />
                </div>
            </For>
        </div>
    }
}

// TODO: configurable backend
fn main() {
    leptos::mount::mount_to_body(App);
}
