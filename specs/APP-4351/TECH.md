# Remote DiffStateModel

Linear: [APP-4351](https://linear.app/warpdotdev/issue/APP-4351/update-diffstatemodel-api)

## Context

`DiffStateModel` (`app/src/code_review/diff_state.rs`) is a per-repo model that owns a `Repository` handle, fs watcher subscription, diff loading, metadata refresh, and mode selection. It emits `DiffStateModelEvent` with four variants: `CurrentBranchChanged`, `NewDiffsComputed`, `SingleFileUpdated`, `MetadataRefreshed`. `CodeReviewView` subscribes to these events and renders diffs identically regardless of how they were produced.

`WorkingDirectoriesModel` (`app/src/pane_group/working_directories.rs`) stores `diff_state_models: HashMap<PathBuf, ModelHandle<DiffStateModel>>` and lazily creates models via `get_or_create_diff_state_model`. `CodeReviewView` holds a `ModelHandle<DiffStateModel>` obtained from this map.

The remote server protocol (`crates/remote_server/proto/remote_server.proto`) uses length-prefixed protobuf over SSH stdio. Push events flow through `ClientEvent` → `RemoteServerManager::forward_client_event` → `RemoteServerManagerEvent` → app-layer subscribers. `ServerModel` (`app/src/remote_server/server_model.rs`) is the daemon-side orchestrator that dispatches `ClientMessage`s and sends `ServerMessage` responses and pushes.

Today `DiffStateModel` only works with local git repositories. To support code review on remote environments (SSH sessions), we need to split the model into local/remote variants behind a wrapper, add proto messages for diff state exchange, build a server-side `GlobalDiffStateModel` that manages diff state per (repo, mode), and implement a client-side `RemoteDiffStateModel` that receives server pushes.

## Proposed changes

### 1. Split DiffStateModel into wrapper + local + remote

Refactor `app/src/code_review/diff_state.rs` into a module directory `app/src/code_review/diff_state/`:
- `mod.rs` — `DiffStateModel` wrapper holding a `DiffStateBackend` enum (`Local` / `Remote`), delegating every read/write method to the active sub-model. Defines `UniversalPath` enum (`Local(PathBuf)` / `Remote(RemoteRepositoryIdentifier)`) used as the cache key in `WorkingDirectoriesModel`.
- `local.rs` — `LocalDiffStateModel` (renamed from the current `DiffStateModel`), retaining all existing behavior.
- `remote.rs` — `RemoteDiffStateModel`, initially a no-op stub with defaults for every read method.

The wrapper subscribes to the active sub-model and re-emits events via `forward_event`. `CodeReviewView` subscribes to the wrapper and renders diffs identically regardless of which backend is active. `DiffStateModelEvent` keeps its name — it's shared between local and remote.

**Additional mechanical changes in the split:**
- Wrap `NewDiffsComputed` payload in `Arc`: `Option<GitDiffWithBaseContent>` → `Option<Arc<GitDiffWithBaseContent>>` for cheap cloning during event forwarding.
- Simplify `DiffState::Loaded` to a unit variant (no inner `GitDiffData`). The diff payload is accessed via `DiffStateModel::get()` or arrives in the event itself.
- Update `WorkingDirectoriesModel`: cache key changes from `HashMap<PathBuf, ...>` to `HashMap<UniversalPath, ...>`, and `get_or_create_diff_state_model` takes `UniversalPath` instead of `PathBuf`. All call sites wrap paths in `UniversalPath::Local(...)`.
- All callers in `code_review_view.rs`, `code_review_header/mod.rs`, `right_panel.rs` pass `ctx` to wrapper methods (the wrapper needs `AppContext` to dereference its inner `ModelHandle`).

### 2. Proto messages

Add to `crates/remote_server/proto/remote_server.proto`.

**Client → Server:**
- `GetDiffState { repo_path, mode }` — request/response. The server responds with a `GetDiffStateResponse` (snapshot or error), then pushes subsequent changes. Follows the `NavigatedToDirectory` → `NavigatedToDirectoryResponse` + `RepoMetadataSnapshot` pattern.
- `UnsubscribeDiffState { repo_path, mode }` — notification (fire-and-forget). Tells the server the client no longer needs updates for this (repo, mode).

**Server → Client:**
- `GetDiffStateResponse` — `oneof result { DiffStateSnapshot snapshot, DiffStateError error }`. Matches the `WriteFileResponse`/`RunCommandResponse` pattern.
- `DiffStateSnapshot` (push) — full state for a (repo, mode). Includes metadata + full `GitDiffData`. Pushed on structural changes (`NewDiffsComputed`).
- `DiffStateMetadataUpdate` (push) — metadata-only update for `MetadataRefreshed` events. Avoids re-serializing the entire diff payload on every 5-second throttled refresh.
- `DiffStateFileDelta` (push) — single-file diff update for `SingleFileUpdated` events. Carries one `FileDiff` + file path + updated metadata. Debounced at 2s on the server.

Wire into `ClientMessage.oneof` (field numbers 13–14) and `ServerMessage.oneof` (field numbers 14–17). Current max field numbers: `ClientMessage` = 12 (`UpdatePreferences`), `ServerMessage` = 13 (`CodebaseIndexStatusUpdated`).

Sub-messages (`DiffModeValue`, `DiffStats`, `GitDiffData`, `FileDiff`, `DiffHunk`, `DiffLine`, `GitFileStatus`) mirror the Rust domain types 1:1. Conversion lives in a new `diff_state_proto.rs`, following the `repo_metadata_proto.rs` pattern.

**No `RefreshDiffMetadata` message** — the server pushes metadata changes automatically via its watcher, matching the `RepoMetadata` pattern.

**No `ChangeDiffMode` message** — mode is immutable per model. Mode changes mean the client unsubscribes old + subscribes new (see §5).

**Rust wire types** (in a new shared module, e.g. `diff_state_wire.rs` — both client and server need these types):

```rust path=null start=null
/// Wire payload for a full diff state snapshot (after routing).
pub struct DiffStateSnapshotData {
    pub metadata: Option<DiffMetadata>,
    pub state: DiffState,
    pub diffs: Option<GitDiffData>,  // present when state is Loaded
}

/// Wire payload for a single-file diff delta (after routing).
pub struct DiffStateFileDeltaData {
    pub path: PathBuf,
    pub diff: Option<FileDiff>,
    pub metadata: Option<DiffMetadata>,
}
```

No new state enum — the existing `DiffState` has the right variants (`NotInRepository`, `Loading`, `Error(String)`, `Loaded`). After §1's changes, `Loaded` is a unit variant (no inner data); the diff payload is carried separately in `DiffStateSnapshotData.diffs`. The proto `oneof state` mirrors the variants directly.

**Wire ↔ event type bridging.** The wire types use `FileDiff` / `GitDiffData`, but the event types use `FileDiffAndContent` / `Arc<GitDiffWithBaseContent>` (which bundle `content_at_head: Option<String>` for the split-diff base editor). `content_at_head` is not serialized over the wire — it would be expensive and is deferred to a separate `GetFileContent` RPC. The remote model wraps on receipt: `FileDiffAndContent { file_diff, content_at_head: None }`, then wraps the full payload in `Arc` before emitting `NewDiffsComputed(Some(Arc::new(...)))`. The `diff_state_proto.rs` conversion layer produces the wire types; the `RemoteDiffStateModel` handles the wrapping step before emitting events.

### 3. Server-side GlobalDiffStateModel

New file: `app/src/remote_server/diff_state_server.rs`.

```rust path=null start=null
#[derive(Hash, Eq, PartialEq, Clone)]
struct ServerDiffStateKey {
    repo_id: RepositoryIdentifier,
    mode: DiffMode,
}

pub struct GlobalDiffStateModel {
    states: HashMap<ServerDiffStateKey, ModelHandle<LocalDiffStateModel>>,
    per_connection_keys: HashMap<ConnectionId, HashSet<ServerDiffStateKey>>,
}
```

**Per-(repo, mode) models with immutable mode.** The server keys models on `(repo_path, mode)`. Each model is pinned to one mode at construction. This is required because the server-side model is shared across connections — mutable mode would let one client's mode switch corrupt another's subscription. Metadata duplication across models for the same repo is acceptable: the git commands are <50ms each, old-mode models are dropped promptly on `UnsubscribeDiffState`, and the `Repository` handle + fs watcher are shared via `DetectedRepositories`.

**Lifecycle:**
1. `GetDiffState` arrives as a request. `GlobalDiffStateModel` looks up or creates a `LocalDiffStateModel` for the key. If already loaded, responds immediately. If loading, uses `ctx.spawn` to respond once `NewDiffsComputed` fires. Reuses `Repository` handles from `DetectedRepositories` (already detected by prior `NavigatedToDirectory`). If no repository has been detected yet (e.g. `GetDiffState` arrives before `NavigatedToDirectory`), responds with `DiffStateError` — the client retries after `NavigatedToDirectory` completes.
2. After responding, subsequent model events are pushed to subscribed connections only via `send_to_diff_state_subscribers(key, msg)`, which scans `per_connection_keys`. Targeted sends avoid broadcasting large diff payloads (~500KB–2MB).
3. `UnsubscribeDiffState` removes the key from the connection's set. If no connection subscribes to the key, the model is dropped.
4. `deregister_connection(conn_id)` removes the connection's entry and drops orphaned models.

**Event → push mapping:**
- `NewDiffsComputed` → full `DiffStateSnapshot`
- `MetadataRefreshed` → `DiffStateMetadataUpdate` (metadata only, no diffs)
- `CurrentBranchChanged` → `DiffStateMetadataUpdate` (metadata only — diffs for the new branch haven't been computed yet; `NewDiffsComputed` follows with actual diffs)
- `SingleFileUpdated` → `DiffStateFileDelta` (debounced at 2s)

### 4. RemoteDiffStateModel implementation

Fill in the no-op stub in `diff_state/remote.rs` (created in §1) to:
- Hold `repo_id: RepositoryIdentifier` (always `Remote` variant), `mode: DiffMode` (immutable), `state: DiffState`, `metadata: Option<DiffMetadata>`.
- Apply incoming `DiffStateSnapshotData`, `DiffStateMetadataUpdate`, and `DiffStateFileDeltaData` from server pushes.
- Wrap `FileDiff` → `FileDiffAndContent { file_diff, content_at_head: None }` and `GitDiffData` → `Arc<GitDiffWithBaseContent>` before emitting events.
- Emit `DiffStateModelEvent` variants matching the server push mapping (§3).

`content_at_head` is `None` for remote files. The view already handles `None` — it skips base-content editor rendering. A separate `GetFileContent` RPC can be added later.

**Required read API surface** (defined in the wrapper's delegation interface):
- Core: `get()`, `diff_mode()`, `get_current_branch_name()`, `get_main_branch_name()`, `get_stats_for_current_mode()`, `get_uncommitted_stats()`, `has_head()`
- Git operations (stubs for v1 — `GitOperationsInCodeReview` won't be enabled for remote): `is_git_operation_blocked()`, `pr_info()`, `is_pr_info_refreshing()`, `is_on_main_branch()`, `unpushed_commits()`, `upstream_ref()`, `upstream_differs_from_main()`
- Mutations: `set_diff_mode()`, `load_diffs_for_current_repo()`, `set_code_review_metadata_refresh_enabled()`, `discard_files()`, `refresh_metadata_and_pr_info()`

For v1, mutation methods that are local-only (`load_diffs_for_current_repo`, `refresh_metadata_and_pr_info`, `set_code_review_metadata_refresh_enabled`) remain no-ops on `RemoteDiffStateModel` — the server drives all state.

### 5. Mode changes and unsubscribe

Since `RemoteDiffStateModel` has immutable mode, mode changes require swapping:
1. Wrapper sends `UnsubscribeDiffState { repo_path, mode: old }`.
2. Creates new `RemoteDiffStateModel` for `(repo_id, new_mode)` in `Loading` state.
3. Wrapper subscribes to the new sub-model's events *before* sending the request (a fast server response could race the subscription otherwise).
4. Sends `GetDiffState { repo_path, mode: new_mode }`.
5. Emits `NewDiffsComputed(None)` so the view shows a loading spinner.
6. Server responds → model transitions to `Loaded`, emits `NewDiffsComputed(Some(...))`.

**Unsubscribe cases:** code review pane close, mode change, repo change (cycling), connection drop, `drop_unused_diff_state_models` (tab close).

### 6. New ClientEvent / RemoteServerManagerEvent variants

`ClientEvent` carries raw proto-derived data (`StandardizedPath`, `DiffMode`). `forward_client_event` in `RemoteServerManager` attaches `host_id`, constructs `RepositoryIdentifier::Remote(...)`, and emits the corresponding manager event. This follows the `RepoMetadataSnapshotReceived` pattern.

Three new variants each for `ClientEvent` and `RemoteServerManagerEvent`:
- `DiffStateSnapshotReceived`
- `DiffStateMetadataUpdateReceived`
- `DiffStateFileDeltaReceived`

`push_message_to_event` in `RemoteServerClient` maps the new `ServerMessage` variants to `ClientEvent` variants.

### 7. WorkingDirectoriesModel integration

After §1's cache key migration, `get_or_create_diff_state_model` accepts `UniversalPath` and the map uses `HashMap<UniversalPath, ModelHandle<DiffStateModel>>`. When a `UniversalPath::Remote(...)` is passed, `DiffStateModel::new` (the wrapper constructor) creates a `RemoteDiffStateModel` in `Loading` state, subscribes to its events, and sends `GetDiffState` to the server — mirroring how the `Local` branch creates and subscribes to `LocalDiffStateModel` today.

## Testing and validation

- **Unit tests** for proto conversion (`diff_state_proto.rs`): round-trip `DiffState` ↔ proto for each variant (`NotInRepository`, `Loading`, `Error`, `Loaded`). Round-trip `DiffMetadata`, `FileDiff`, `DiffHunk`, `DiffLine`, `GitFileStatus`.
- **Unit tests** for `GlobalDiffStateModel`: subscribe/unsubscribe lifecycle, multi-connection dedup, orphan model cleanup on `deregister_connection`.
- **Unit tests** for `RemoteDiffStateModel`: snapshot application, metadata update, file delta patch (insert/replace/remove), mode change swap.
- **Integration test**: connect two clients to same repo with different modes → verify independent push streams and no cross-contamination.
- **Manual**: open code review on an SSH session, verify diffs load, mode switching works, file saves produce incremental updates, closing the pane unsubscribes.

## Risks and mitigations

- **Large snapshot payloads.** A repo with 50 changed files can produce 500KB–2MB snapshots. Mitigation: `DiffStateFileDelta` handles single-file changes (~5–20KB); full snapshots are only sent on structural changes. Targeted sends via `per_connection_keys` avoid broadcasting to unsubscribed connections.
- **Out-of-order delta/snapshot.** A stale delta arriving after a full snapshot could show incorrect state. Mitigation: full snapshots are always authoritative — they overwrite cached state. The next delta or snapshot corrects any transient inconsistency.
- **Reconnection.** When `SessionReconnected` fires, `RemoteDiffStateModel` must re-send `GetDiffState` to re-establish its subscription on the (potentially restarted) server. This should be driven by the app-layer `RemoteServerManagerEvent::SessionReconnected` handler.

## Deferred RPCs (not in v1)

- `DiscardFiles` — request/response for `discard_files()`. Runs `git restore`/`git stash`/`git rm` on the remote filesystem. Gated on `GitOperationsInCodeReview`.
- `GetAllBranches` — request/response for the branch selector dropdown. Until wired, the selector shows only Head + MainBranch modes.

## Parallelization

This work splits into four tracks:
1. **Wrapper split (§1)** — refactor `diff_state.rs` into `diff_state/` module, create wrapper + local + remote stub, migrate callers. No proto or server dependencies.
2. **Proto + conversion layer (§2)** — `.proto` definitions, `diff_state_proto.rs`, sub-message types. No app-layer dependencies. Can proceed in parallel with track 1.
3. **Server-side `GlobalDiffStateModel` (§3)** — `diff_state_server.rs`, `ServerModel` handler wiring. Depends on tracks 1 + 2.
4. **Client-side `RemoteDiffStateModel` implementation (§4–§7)** — fill in the stub with snapshot/delta/metadata application, wire `ClientEvent`/`RemoteServerManagerEvent` variants, connect `WorkingDirectoriesModel` remote key path. Depends on tracks 1 + 2.

Tracks 3 and 4 can proceed in parallel once tracks 1 and 2 land.
