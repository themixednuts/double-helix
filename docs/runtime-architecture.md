# Runtime Architecture

Helix terminal UI events are split into two paths:

```text
terminal / host input
  -> compositor frame handling
     -> EventResult + typed PostAction
     -> compositor::apply_post_action
  -> render

async/runtime producers
  -> RuntimeIngress
  -> RuntimeDelivery
     -> Status
     -> Timer
     -> Task(RuntimeTaskEvent)
     -> Ui(UiCommand)
  -> effect::apply_task_event / runtime::ui::apply_ui_command
  -> render
```

`RuntimeIngress` is the typed, bounded mailbox used by background work,
timers, plugin hosts, and async UI producers. Producers enqueue one
`RuntimeDelivery`; the application main loop consumes deliveries on the editor
thread and applies them before the next render.

## Delivery Types

`RuntimeDelivery::Status` updates editor status/error messages.

`RuntimeDelivery::Timer` delivers host timer expiry.

`RuntimeDelivery::Task(RuntimeTaskEvent)` is for editor-side effects and async
task results. The task effect layer lives in `helix-term/src/effect.rs` and
feature modules under `helix-term/src/effect/`.

`RuntimeDelivery::Ui(UiCommand)` is for compositor-facing UI requests. The UI
apply layer lives in `helix-term/src/runtime/ui/apply.rs`; feature payloads
live under `helix-term/src/runtime/ui/command/` and apply modules under
`helix-term/src/runtime/ui/`.

## Frontend Boundary

A non-terminal frontend should provide the host services represented by
`helix-term/src/host.rs`:

- `UiHost::invalidate` marks all or part of the surface dirty.
- `UiHost::request_timer` schedules a timer and sends expiry through ingress.

The frontend owns platform input, timer scheduling, render presentation, and
its event loop. The engine-side code owns editor state mutation, typed runtime
delivery application, component event handling, and render output generation.

The frontend must consume `RuntimeIngressReceiver` and apply deliveries on the
same editor thread that owns `Editor` and `Compositor`. `RuntimeTaskEvent`
continues through the effect layer. `UiCommand` continues through the UI apply
layer. This keeps async producers from mutating UI state directly.

## Plugin UI

Plugin-originated terminal UI requests enter through `UiCommand::Plugin`.
Prompt, confirm, picker, notification, and panel display mutations use the
same ingress/apply path as other UI requests. Plugin UI callback results are
converted to `RuntimeTaskEvent::DeliverPluginUiCallback` so the plugin engine
receives them on the editor side.

Panel registration still allocates a model panel synchronously because the
plugin host contract returns a panel handle immediately. Panel component
creation is then sent as `UiCommand::Plugin(PluginCommand::PushPanel)`.

## PostAction

`PostAction` is the frame-local request type returned by synchronous component
event handling. It is data, not arbitrary executable behavior. Components may
request:

- `PopLayer`
- `RemoveById`
- `PushLayer`
- `ReplaceOrPushLayer`
- `UpdateCompletionFilter`
- `ClearCompletion`
- `ShowCommandPalette`
- `RestoreLastPicker`
- `ReplayKeys`
- `Batch`

`PushLayer` and `ReplaceOrPushLayer` carry a boxed component because layer
construction is inherently frontend component data. All application behavior
for these requests is centralized in `compositor::apply_post_action`, and the
dispatcher runs synchronously in the same event-handling pass that produced the
request.
