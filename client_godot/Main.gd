## Entry point: builds the scene tree in code and wires the networking signals to
## the world, the entity manager, the local player, and the UI.
##
## Flow (mirrors docs/protocol.md): connect -> auth_required -> (resume token, or
## login/register/guest) -> auth_ok (store token) -> welcome (spawn) -> partition
## (draw districts) -> status_update stream (move/see others).
extends Node3D

const SESSION_PATH := "user://session.cfg"
const GATEWAY_URL := "ws://127.0.0.1:8766"

## Editor mode (terrain editing #78): launch with `--editor-mode` (after the
## `--` separator, e.g. `Godot --path client_godot -- --editor-mode`) to get
## the free-fly camera + terrain brush instead of the player, logged in as
## the server-seeded editor account (dev credentials, mirroring proxy.rs)
## unless overridden with `--editor-email=<email> --editor-pass=<password>`
## — any editor-role account works, and edits are then attributed to it.
const EDITOR_EMAIL := "editor@capital.town"
const EDITOR_PASSWORD := "editor12345"

var _net: NetworkClient
var _world: World
var _streamer: TerrainStreamer
var _entities: EntityManager
var _player: LocalPlayer
var _login: Login
var _hud: Hud
var _vitals: VitalsHud
var _minimap: Minimap
var _storage: StoragePanel
var _inventory: InventoryPanel
var _build: BuildPanel
var _skills: SkillsPanel
var _craft: CraftPanel
var _build_place: BuildPlace
var _mayor_road: MayorRoad
var _rent: RentPanel
var _transition: DistrictTransition

var _editor_mode := false
var _editor_cam: EditorCamera
## The camera is first placed at `_setup_editor` (on `welcome`), but the real
## world size only arrives with the `partition` message moments later — the
## first partition recentres it once over the true town centre, then leaves
## the camera alone (later partitions are zone splits/merges, and yanking the
## camera mid-flight would be hostile).
var _editor_cam_centred := false
var _brush: BrushController
var _history: HistoryPanel
var _object_tool: ObjectTool
var _road_tool: RoadTool
var _demolish_tool: DemolishTool
var _toolbar: EditorToolbar
var _world_objects: WorldObjects

var _my_id := ""
var _plot_id := ""
var _plot_bounds: Dictionary = {}
var _current_district := ""
var _seeded_position := false
## skill_id -> level, mirrored to the build board so it can grey gated orders.
var _skill_levels: Dictionary = {}
var _sleep_down := false
var _rent_panel_down := false

func _ready() -> void:
    _editor_mode = OS.get_cmdline_user_args().has("--editor-mode") \
        or OS.get_cmdline_args().has("--editor-mode")
    _build_environment()

    _world = World.new()
    add_child(_world)

    _streamer = TerrainStreamer.new()
    _world.add_child(_streamer)

    _entities = EntityManager.new()
    add_child(_entities)

    # Placed world props (#86) — every client renders them, editor or not.
    _world_objects = WorldObjects.new()
    add_child(_world_objects)

    _player = LocalPlayer.new()
    _player.visible = false
    add_child(_player)

    _hud = Hud.new()
    add_child(_hud)

    _vitals = VitalsHud.new()
    add_child(_vitals)

    _minimap = Minimap.new()
    add_child(_minimap)

    _storage = StoragePanel.new()
    _storage.visible = false
    add_child(_storage)

    _inventory = InventoryPanel.new()
    _inventory.visible = false
    add_child(_inventory)

    _build = BuildPanel.new()
    _build.visible = false
    add_child(_build)

    _skills = SkillsPanel.new()
    add_child(_skills)

    _craft = CraftPanel.new()
    _craft.visible = false
    add_child(_craft)

    _build_place = BuildPlace.new()
    add_child(_build_place)

    _mayor_road = MayorRoad.new()
    add_child(_mayor_road)

    _rent = RentPanel.new()
    add_child(_rent)

    _transition = DistrictTransition.new()
    add_child(_transition)

    _login = Login.new()
    _login.visible = false
    add_child(_login)

    _net = NetworkClient.new()
    add_child(_net)

    _wire_signals()
    _net.connect_to(GATEWAY_URL)

func _process(_delta: float) -> void:
    # Open the storage / build panels only while standing near their fixtures. The
    # inventory panel is toggled with I, but is also auto-shown near storage so items
    # can be dragged straight into it.
    if _my_id == "":
        return
    if _editor_mode:
        # The free-fly camera drives tile streaming instead of the player;
        # none of the proximity gameplay below applies to an editor.
        if _editor_cam != null:
            var cam_pos := _editor_cam.world_pos()
            _streamer.on_player_position(cam_pos.x, cam_pos.y)
        return
    var near_store := _entities.nearest_storage(_player.world_pos(), Protocol.STORAGE_RANGE) != ""
    _storage.show_panel(near_store)
    _inventory.set_forced_open(near_store)
    var near_board := _entities.nearest_build_board(_player.world_pos(), Protocol.BOARD_RANGE) != ""
    _build.show_panel(near_board)
    var near_craft := _entities.nearest_crafting(_player.world_pos(), Protocol.STORAGE_RANGE) != ""
    _craft.show_panel(near_craft)

    # Feed the placement ghost the camera (to raycast the mouse onto the
    # ground), the player's own plot bounds, and the live entity roster (both
    # needed for its red/green validity preview), and offer "sleep / set
    # respawn" while standing near a bed (#12).
    _build_place.camera = _player.camera()
    _build_place.plot_bounds = _plot_bounds
    _build_place.entities = _entities
    _mayor_road.camera = _player.camera()
    var near_bed := _entities.nearest_bed(_player.world_pos(), Protocol.STORAGE_RANGE)
    var sleep := Input.is_physical_key_pressed(KEY_F)
    if sleep and not _sleep_down and near_bed != "":
        _net.send_home_set_respawn(near_bed)
    _sleep_down = sleep

    # Rent isn't tied to standing at a fixture — toggle the panel with a keypress.
    var rent_key := Input.is_physical_key_pressed(KEY_P)
    if rent_key and not _rent_panel_down:
        _rent.show_panel(not _rent.visible)
    _rent_panel_down = rent_key

func _build_environment() -> void:
    var env := Environment.new()
    env.background_mode = Environment.BG_COLOR
    # A hazy daylight sky the distance fog can fade into — with the old
    # near-black background, fogged terrain read as a wrong-looking dark
    # band instead of receding into the sky.
    env.background_color = Color(0.55, 0.63, 0.72)
    env.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
    env.ambient_light_color = Color(0.5, 0.5, 0.55)
    env.ambient_light_energy = 0.6
    # Distance haze over the metric 25.6km world: barely-there inside the
    # streamed fine-tile ring (~18% at 2km), heavy by the far districts
    # (~70% at 12km), so the coarse backdrop beyond the ring reads as
    # terrain-in-the-haze rather than a smooth low-poly leftover.
    env.fog_enabled = true
    env.fog_light_color = Color(0.55, 0.63, 0.72) # match the sky: fade into it
    env.fog_density = 0.0001
    env.fog_sun_scatter = 0.05
    var we := WorldEnvironment.new()
    we.environment = env
    add_child(we)

    var sun := DirectionalLight3D.new()
    sun.rotation_degrees = Vector3(-55, -40, 0)
    sun.light_energy = 1.1
    add_child(sun)

func _wire_signals() -> void:
    _net.opened.connect(func(): _hud.set_conn("connected"))
    _net.closed.connect(func(): _hud.set_conn("disconnected"))
    _net.auth_required.connect(_on_auth_required)
    _net.auth_ok.connect(_on_auth_ok)
    _net.auth_error.connect(_on_auth_error)
    _net.welcome.connect(_on_welcome)
    _net.partition.connect(func(msg):
        _world.apply_partition(msg)
        _streamer.set_context(_world._zones, _world.world_size)
        _player.set_world_size(float(msg.get("world", 6400)))
        if _editor_mode and _editor_cam != null and not _editor_cam_centred:
            _editor_cam_centred = true
            var mid := _world.world_size * 0.5
            _editor_cam.place_over(mid, mid))
    _net.terrain_data.connect(func(resolution, world_size, heights):
        Protocol.apply_terrain_data(resolution, world_size, heights)
        _world.on_terrain_data()
        # Kick-start tile streaming at the spawn position: `position_changed`
        # fired at activate() before the tile-grid shape was known (a no-op),
        # and it won't fire again until the player actually moves — without
        # this, a player standing still at spawn never streams any tiles.
        # (In editor mode the free-fly camera is the streaming anchor.)
        var anchor := _editor_cam.world_pos() if _editor_mode and _editor_cam != null else _player.world_pos()
        _streamer.on_player_position(anchor.x, anchor.y))
    _net.terrain_tile_data.connect(func(tx, ty, heights): _streamer.on_tile_data(tx, ty, heights))
    _streamer.tile_requested.connect(func(tx, ty): _net.send_terrain_tile_request(tx, ty))
    # Plot markers / roads are static meshes sampled against the terrain at
    # draw time — redraw them whenever the displayed surface changes so they
    # sit on the ground instead of staying buried under streamed-in or
    # brush-raised terrain.
    _streamer.terrain_changed.connect(_world.refresh_plot_markers)
    # Placed props ground-snap the same way plot markers do: re-snap whenever
    # the displayed surface changes (streamed tiles in/out, accepted edits).
    _streamer.terrain_changed.connect(_world_objects.refresh_heights)
    # Hide the coarse backdrop under every resident fine tile: its ~66m
    # interpolation runs metres above the true 5m ground in places, and
    # without the mask it renders as a phantom surface swallowing the
    # player wherever it wins the depth test.
    _streamer.terrain_changed.connect(func():
        _world.update_backdrop_mask(_streamer.resident_tiles()))
    # Hand-authored edit layer (terrain editing #72): requested alongside
    # each tile, composited onto the tile's heights before/at mesh build.
    _net.terrain_delta_data.connect(func(tx, ty, has_delta, offsets): _streamer.on_delta_data(tx, ty, has_delta, offsets))
    _streamer.delta_requested.connect(func(tx, ty): _net.send_terrain_delta_request(tx, ty))
    # Accepted edits (anyone's) arrive as authoritative per-chunk patches;
    # a rejected own-edit rolls its preview back to the last known state.
    _net.terrain_delta_patch.connect(func(tx, ty, _rev, offsets): _streamer.on_delta_patch(tx, ty, offsets))
    _net.terrain_edit_error.connect(func(message):
        _hud.flash_announce("Editor: %s" % message)
        _streamer.discard_edit_preview())
    _net.status_update.connect(_on_status_update)
    _net.plot_district.connect(func(plots):
        _world.apply_plot_roster(plots, _plot_id)
        _minimap.set_plots(plots, _plot_id))
    _net.despawn.connect(func(id):
        _entities.remove(id)
        _world.remove_dirt_road(id)) # a demolished road's ribbon (#107)
    _net.zone_migration.connect(func(zone): _hud.set_zone(zone))
    # Death gets a proper overlay (#89) instead of the old connection-label
    # hack; the respawn's status stream restores the vitals bars underneath.
    _net.you_died.connect(func(): _vitals.show_death())
    _net.gather_progress.connect(func(_node_id, pct): _hud.set_gather_progress(pct))
    _net.gather_result.connect(func(item_id, qty): _hud.flash_gain(item_id, qty))
    _net.inv_update.connect(func(items, used, capacity):
        _hud.set_inventory(items, used, capacity)
        _storage.set_inventory(items)
        _inventory.set_inventory(items, used, capacity)
        _build.set_inventory(items)
        _craft.set_inventory(items))
    _net.skill_update.connect(_on_skill_update)
    _net.skill_levelup.connect(func(skill_id, level): _hud.flash_levelup(skill_id, level))
    _net.store_update.connect(func(items): _storage.set_storage(items))
    _net.build_list.connect(func(orders):
        _build.set_orders(orders)
        # Staked road plans (#95): every client renders open road orders'
        # paths so players can see where stone is wanted.
        _world.apply_road_plans(orders))
    _net.build_progress.connect(func(order_id, required, progress): _build.update_progress(order_id, required, progress))
    _net.build_completed.connect(func(order_id, _structures):
        _build.mark_completed(order_id)
        _world.remove_road_plan(order_id)) # the built road (#96) takes over
    _net.build_unlocked.connect(func(_ids): _net.send_build_list())
    _net.plot_assigned.connect(_on_plot_assigned)
    _net.build_placed.connect(func(structure): _hud.flash_announce("Placed %s" % String(structure.get("kind", ""))))
    _net.craft_recipes.connect(func(recipes): _craft.set_recipes(recipes))
    _net.craft_made.connect(func(_recipe_id, item_id, qty): _hud.flash_gain(item_id, qty))
    _net.home_respawn_set.connect(func(_bed_id): _hud.flash_announce("Respawn point set!"))
    _net.rent_status.connect(_on_rent_status)
    _net.rent_warning.connect(func(_plot_id_arg, due_at): _hud.flash_announce(
        "Rent is due soon (in %dh) — press P to pay" % maxi((due_at - int(Time.get_unix_time_from_system())) / 3600, 0)))
    _net.rent_reclaimed.connect(func(_plot_id_arg, _moved): _hud.flash_announce(
        "Your plot lapsed — your belongings are safe in storage, flair untouched."))
    _net.district_ready.connect(func(): _transition.mark_server_ready())
    # Placed world props (#86): the roster answer plus the live broadcasts.
    _net.object_list.connect(func(objects): _world_objects.apply_list(objects))
    _net.object_placed.connect(func(id, kind, x, y): _world_objects.on_placed(id, kind, x, y))
    _net.object_removed.connect(func(id): _world_objects.on_removed(id))

    _storage.do_deposit.connect(func(item_id, qty): _net.send_store_deposit(item_id, qty))
    _storage.do_withdraw.connect(func(item_id, qty): _net.send_store_withdraw(item_id, qty))
    _inventory.do_withdraw.connect(func(item_id, qty): _net.send_store_withdraw(item_id, qty))
    _build.do_contribute.connect(func(order_id, item_id, qty): _net.send_build_contribute(order_id, item_id, qty))
    _craft.do_craft.connect(func(recipe_id): _net.send_craft_make(recipe_id))
    _build_place.do_place.connect(func(kind, x, y, rot): _net.send_build_place(kind, x, y, rot))
    _build_place.mode_changed.connect(func(active, kind, rot): _hud.set_build_hint(active, kind, rot))
    _mayor_road.do_create.connect(_on_mayor_road_create)
    _mayor_road.mode_changed.connect(func(active, has_start):
        if active:
            _hud.flash_announce("Mayor: click the end point" if has_start else "Mayor: click the start point"))
    _net.mayor_build_error.connect(func(message): _hud.flash_announce("Mayor: %s" % message))
    _rent.do_pay.connect(func(plot_id): _net.send_rent_pay(plot_id))
    _rent.do_set_autopay.connect(func(plot_id, enabled): _net.send_rent_set_autopay(plot_id, enabled))

    _login.do_login.connect(func(email, pw): _save_email(email); _net.login(email, pw))
    _login.do_register.connect(func(email, pw, cname): _save_email(email); _net.register(email, pw, cname))
    _login.do_guest.connect(func(): _net.guest())

    _player.move_requested.connect(func(dx, dy): _net.send_move(dx, dy))
    _player.attack_requested.connect(func(dx, dy): _net.send_attack(dx, dy))
    _player.gather_pressed.connect(_on_gather_pressed)
    _player.position_changed.connect(func(wx, wy):
        _hud.set_pos(wx, wy)
        _minimap.set_player(wx, wy, _player.facing())
        _streamer.on_player_position(wx, wy)
        _check_district_crossing(wx, wy))

## Fan a skill update out to the HUD glance line, the skills panel (progress bars),
## and the build board (which greys orders above the player's current level).
func _on_skill_update(skill_id: String, xp: int, level: int) -> void:
    _hud.set_skill(skill_id, xp, level)
    _skills.set_skill(skill_id, xp, level)
    _skill_levels[skill_id] = level
    _build.set_skill_levels(_skill_levels)

# --- handshake ----------------------------------------------------------------

func _on_auth_required(version: int) -> void:
    if _editor_mode:
        # Editor mode skips the login UI (and any saved session token —
        # a stale player token must not shadow the editor identity).
        # `--editor-email=` / `--editor-pass=` pick a different editor-role
        # account (edit provenance is attributed to whoever logs in here);
        # default is the server-seeded dev editor.
        var email := EDITOR_EMAIL
        var password := EDITOR_PASSWORD
        for arg in OS.get_cmdline_user_args():
            if arg.begins_with("--editor-email="):
                email = arg.substr(len("--editor-email="))
            elif arg.begins_with("--editor-pass="):
                password = arg.substr(len("--editor-pass="))
        _login.show_overlay(false)
        _net.login(email, password)
        return
    _login.set_version(version)
    _login.prefill_email(_load_email())
    var token := _load_token()
    if token != "":
        _login.show_overlay(false)
        _net.resume_token(token) # silent reconnect
    else:
        _login.show_overlay(true)

func _on_auth_ok(data: Dictionary) -> void:
    var token := String(data.get("token", ""))
    if token != "":
        _save_token(token)

func _on_auth_error(message: String) -> void:
    _clear_token() # a stale token won't resume
    _login.set_error(message)

func _on_welcome(data: Dictionary) -> void:
    _my_id = String(data.get("player_id", ""))
    _seeded_position = false
    _entities.set_local_id(_my_id)
    _hud.set_zone(String(data.get("zone", "—")))
    _login.show_overlay(false)
    if _editor_mode:
        _setup_editor()
    else:
        _player.activate()
    _net.send_craft_list() # the recipe registry is static; pull it once per session
    _net.send_terrain_list() # the heightmap is static; pull it once per session
    _net.send_object_list() # placed props: roster once, then broadcasts keep it live
    _mayor_road.is_mayor = String(data.get("role", "player")) == "mayor"
    if _mayor_road.is_mayor:
        _hud.flash_announce("You are the mayor — press M to commission a dirt path")

## Build the editor rig (terrain editing #78): free-fly camera over the town
## centre + the height brush, replacing the player entirely (the character
## the editor account owns just idles at spawn, server-side).
func _setup_editor() -> void:
    _editor_cam = EditorCamera.new()
    add_child(_editor_cam)
    # Provisional until the first `partition` supplies the real world size
    # (see `_editor_cam_centred`) — `welcome` arrives just before it.
    var mid := _world.world_size * 0.5
    _editor_cam.place_over(mid, mid)
    _editor_cam.make_current()
    _brush = BrushController.new()
    _brush.camera = _editor_cam
    _brush.streamer = _streamer
    add_child(_brush)
    _brush.stroke_committed.connect(func(brush, cells): _net.send_terrain_edit_op(brush, cells))
    # (Brush status routes to the toolbar hint line below, #103.)
    _history = HistoryPanel.new()
    add_child(_history)
    _history.do_revert.connect(func(op_id): _net.send_terrain_revert_op(op_id))
    _net.terrain_edit_ack.connect(func(op_id, brush): _history.record_op(op_id, brush))
    _net.terrain_revert_ack.connect(func(op_id): _history.mark_reverted(op_id))
    # Object placement tool (#86): [O] cycles off/place/delete; while it's
    # on, the terrain brush yields the mouse (a placement click must not
    # also carve the ground). Placement renders via the server broadcast —
    # the tool itself never touches WorldObjects' contents.
    _object_tool = ObjectTool.new()
    _object_tool.camera = _editor_cam
    _object_tool.objects = _world_objects
    add_child(_object_tool)
    _object_tool.place_requested.connect(func(kind, x, y): _net.send_object_place(kind, x, y))
    _object_tool.delete_requested.connect(func(object_id): _net.send_object_delete(object_id))
    _net.object_edit_error.connect(func(message): _hud.flash_announce("Editor: %s" % message))
    # Road tool (#95): [R] toggles grid-snapped road laying; committed plans
    # go up as one road.plan and come back as a staked build order.
    _road_tool = RoadTool.new()
    _road_tool.camera = _editor_cam
    _road_tool.world_ref = _world # staked-plan source for move-mode picking (#105)
    add_child(_road_tool)
    _road_tool.plan_committed.connect(func(points): _net.send_road_plan(points))
    _road_tool.replan_committed.connect(func(order_id, points): _net.send_road_replan(order_id, points))
    _net.road_planned.connect(func(_order_id): _hud.flash_announce("Road: plan accepted — stone wanted!"))
    _net.road_plan_error.connect(func(message): _hud.flash_announce("Road: %s" % message))
    # The toolbar (#103) owns tool exclusivity — one active-tool state
    # drives the whole enabled matrix, buttons and hotkeys converge on it,
    # and the tools' status streams show in its persistent hint line
    # instead of scrolling away as announce toasts.
    # Demolish tool (#107): cancel pristine plans free; anything with stone
    # in it gets a salvage job that refunds on completion.
    _demolish_tool = DemolishTool.new()
    _demolish_tool.camera = _editor_cam
    _demolish_tool.world_ref = _world
    add_child(_demolish_tool)
    _demolish_tool.cancel_requested.connect(func(order_id): _net.send_road_cancel(order_id))
    _demolish_tool.demolish_requested.connect(func(order_id): _net.send_road_demolish(order_id))
    _net.road_cancelled.connect(func(_order_id): _hud.flash_announce("Road: plan cancelled"))
    _net.road_demolition_planned.connect(func(_order_id, _demo_id): _hud.flash_announce("Road: demolition posted — bring a tool kit!"))
    _toolbar = EditorToolbar.new()
    add_child(_toolbar)
    _toolbar.setup(_brush, _object_tool, _road_tool, _demolish_tool, _history)
    _demolish_tool.status_changed.connect(func(text): _toolbar.set_hint(text))
    _object_tool.status_changed.connect(func(text): _toolbar.set_hint(text))
    _road_tool.status_changed.connect(func(text): _toolbar.set_hint(text))
    _brush.status_changed.connect(func(text): _toolbar.set_hint(text))
    _hud.flash_announce("EDITOR — RMB-drag look, WASD/QE fly; tools on the toolbar")

## A mayor-drawn dirt path (#55): pick the district from its start point and
## commission it with a flat cost — any player can then fill it, same as any
## other build order. `kind` just needs to be unique per order (there's no
## authored tech tree to key into here).
func _on_mayor_road_create(x0: int, y0: int, x1: int, y1: int) -> void:
    var district := _world.district_at(x0, y0)
    if district == "":
        return
    var kind := "dirt_path_%d_%d" % [Time.get_ticks_msec(), randi() % 100000]
    _net.send_mayor_build_create(district, kind, "dirt_road", "{\"stone\":5}", x0, y0, x1, y1)

## A starter plot was (re-)assigned: remember its id (for rent.pay/autopay),
## draw its outline/beacon in the world, feed the HUD compass, and — only on
## the very first grant — flash the onboarding banner (#11).
func _on_plot_assigned(plot_id: String, district: String, bounds: Dictionary, _tier: int, just_claimed: bool) -> void:
    _plot_id = plot_id
    _plot_bounds = bounds
    _world.show_home_plot(bounds)
    var cx := float(bounds.get("x", 0)) + float(bounds.get("w", 0)) * 0.5
    var cy := float(bounds.get("y", 0)) + float(bounds.get("h", 0)) * 0.5
    _hud.set_home(cx, cy)
    _minimap.set_home(cx, cy)
    if just_claimed:
        _hud.flash_announce("Your home plot is ready, in the %s!" % district.capitalize())

## Feed a `rent.status` push (login, pay, auto-pay toggle, or a ticker-driven
## change) to both the HUD's compact hint and the toggleable rent panel (#14).
func _on_rent_status(plot_id: String, due_at: int, paid_through: int, state: String, auto_pay: bool, gold: int) -> void:
    _hud.set_rent_hint(state, due_at)
    _rent.set_status(plot_id, due_at, paid_through, state, auto_pay, gold)

## The client already knows every zone's district from `partition`, so it
## detects a gate crossing itself (comparing the live position against those
## tiles) rather than waiting on the server. Shows the transition curtain and
## announces it (`district.enter`); the actual position/zone handoff already
## happened via the ordinary seamless migrate-request path (#15). The first
## district assignment (on spawn/reconnect) is silent — nothing to "transition"
## into yet.
func _check_district_crossing(wx: float, wy: float) -> void:
    var d := _world.district_at(wx, wy)
    if d == "" or d == _current_district:
        return
    var from_district := _current_district
    _current_district = d
    _minimap.set_district_bounds(_world.district_rect_at(wx, wy))
    if from_district == "":
        return # first assignment this session — no curtain
    _transition.begin(d)
    _net.send_district_enter(from_district, d)

## Gather the nearest in-range resource node (resolved from the entity manager).
func _on_gather_pressed() -> void:
    var node_id := _entities.nearest_resource(_player.world_pos(), Protocol.GATHER_RANGE)
    if node_id != "":
        _net.send_gather_start(node_id)

func _on_status_update(id: String, zone: String, state: Dictionary) -> void:
    if id == _my_id:
        if _editor_mode:
            return # the editor's idle character isn't controlled or rendered
        # Vitals (#89): server-authoritative hp/breath/poison, straight off
        # the wire — the HUD never predicts drain rates.
        _vitals.set_vitals(
            int(state.get("hp", 100)), int(state.get("max_hp", 100)),
            int(state.get("breath", 0)), int(state.get("max_breath", 1)),
            bool(state.get("submerged", false)),
            int(state.get("poison_buildup", 0)), int(state.get("max_poison", 1)),
            bool(state.get("poisoned", false)))
        if not _seeded_position:
            # First authoritative snapshot: place us exactly where the server spawned us.
            _seeded_position = true
            _player.activate(Vector2(float(state.get("x", 600)), float(state.get("y", 600))))
        else:
            _player.reconcile(state)
        if zone != "":
            _hud.set_zone(zone)
    elif String(state.get("type", "")) == "structure" and String(state.get("kind", "")) == "dirt_road":
        # A segment structure (start + end point), not a single-point one — the
        # world (which owns terrain sampling) renders it as a ribbon, not the
        # generic single-point block `EntityManager` draws for other structures.
        _world.upsert_dirt_road(id, state)
    else:
        _entities.upsert(id, zone, state)

# --- session persistence (token + last email) ---------------------------------

func _config() -> ConfigFile:
    var cfg := ConfigFile.new()
    cfg.load(SESSION_PATH) # ignore error: a missing file is just an empty config
    return cfg

func _load_token() -> String:
    return String(_config().get_value("session", "token", ""))

func _save_token(token: String) -> void:
    var cfg := _config()
    cfg.set_value("session", "token", token)
    cfg.save(SESSION_PATH)

func _clear_token() -> void:
    var cfg := _config()
    cfg.erase_section_key("session", "token")
    cfg.save(SESSION_PATH)

func _load_email() -> String:
    return String(_config().get_value("session", "email", ""))

func _save_email(email: String) -> void:
    var cfg := _config()
    cfg.set_value("session", "email", email)
    cfg.save(SESSION_PATH)
