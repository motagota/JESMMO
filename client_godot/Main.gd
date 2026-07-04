## Entry point: builds the scene tree in code and wires the networking signals to
## the world, the entity manager, the local player, and the UI.
##
## Flow (mirrors docs/protocol.md): connect -> auth_required -> (resume token, or
## login/register/guest) -> auth_ok (store token) -> welcome (spawn) -> partition
## (draw districts) -> status_update stream (move/see others).
extends Node3D

const SESSION_PATH := "user://session.cfg"
const GATEWAY_URL := "ws://127.0.0.1:8766"

var _net: NetworkClient
var _world: World
var _entities: EntityManager
var _player: LocalPlayer
var _login: Login
var _hud: Hud
var _storage: StoragePanel
var _inventory: InventoryPanel
var _build: BuildPanel
var _skills: SkillsPanel
var _craft: CraftPanel
var _build_place: BuildPlace
var _rent: RentPanel

var _my_id := ""
var _plot_id := ""
var _seeded_position := false
## skill_id -> level, mirrored to the build board so it can grey gated orders.
var _skill_levels: Dictionary = {}
var _sleep_down := false
var _rent_panel_down := false

func _ready() -> void:
    _build_environment()

    _world = World.new()
    add_child(_world)

    _entities = EntityManager.new()
    add_child(_entities)

    _player = LocalPlayer.new()
    _player.visible = false
    add_child(_player)

    _hud = Hud.new()
    add_child(_hud)

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

    _rent = RentPanel.new()
    add_child(_rent)

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
    var near_store := _entities.nearest_storage(_player.world_pos(), Protocol.STORAGE_RANGE) != ""
    _storage.show_panel(near_store)
    _inventory.set_forced_open(near_store)
    var near_board := _entities.nearest_build_board(_player.world_pos(), Protocol.BOARD_RANGE) != ""
    _build.show_panel(near_board)
    var near_craft := _entities.nearest_crafting(_player.world_pos(), Protocol.STORAGE_RANGE) != ""
    _craft.show_panel(near_craft)

    # Keep the placement ghost following the player, and offer "sleep / set
    # respawn" while standing near a bed (#12).
    _build_place.player_pos = _player.world_pos()
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
    env.background_color = Color(0.04, 0.05, 0.07)
    env.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
    env.ambient_light_color = Color(0.5, 0.5, 0.55)
    env.ambient_light_energy = 0.6
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
    _net.partition.connect(func(msg): _world.apply_partition(msg))
    _net.status_update.connect(_on_status_update)
    _net.despawn.connect(func(id): _entities.remove(id))
    _net.zone_migration.connect(func(zone): _hud.set_zone(zone))
    _net.you_died.connect(func(): _hud.set_conn("you died — respawning…"))
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
    _net.build_list.connect(func(orders): _build.set_orders(orders))
    _net.build_progress.connect(func(order_id, required, progress): _build.update_progress(order_id, required, progress))
    _net.build_completed.connect(func(order_id, _structures): _build.mark_completed(order_id))
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

    _storage.do_deposit.connect(func(item_id, qty): _net.send_store_deposit(item_id, qty))
    _storage.do_withdraw.connect(func(item_id, qty): _net.send_store_withdraw(item_id, qty))
    _inventory.do_withdraw.connect(func(item_id, qty): _net.send_store_withdraw(item_id, qty))
    _build.do_contribute.connect(func(order_id, item_id, qty): _net.send_build_contribute(order_id, item_id, qty))
    _craft.do_craft.connect(func(recipe_id): _net.send_craft_make(recipe_id))
    _build_place.do_place.connect(func(kind, x, y, rot): _net.send_build_place(kind, x, y, rot))
    _build_place.mode_changed.connect(func(active, kind, rot): _hud.set_build_hint(active, kind, rot))
    _rent.do_pay.connect(func(plot_id): _net.send_rent_pay(plot_id))
    _rent.do_set_autopay.connect(func(plot_id, enabled): _net.send_rent_set_autopay(plot_id, enabled))

    _login.do_login.connect(func(email, pw): _save_email(email); _net.login(email, pw))
    _login.do_register.connect(func(email, pw, cname): _save_email(email); _net.register(email, pw, cname))
    _login.do_guest.connect(func(): _net.guest())

    _player.move_requested.connect(func(dx, dy): _net.send_move(dx, dy))
    _player.attack_requested.connect(func(dx, dy): _net.send_attack(dx, dy))
    _player.gather_pressed.connect(_on_gather_pressed)
    _player.position_changed.connect(func(wx, wy): _hud.set_pos(wx, wy))

## Fan a skill update out to the HUD glance line, the skills panel (progress bars),
## and the build board (which greys orders above the player's current level).
func _on_skill_update(skill_id: String, xp: int, level: int) -> void:
    _hud.set_skill(skill_id, xp, level)
    _skills.set_skill(skill_id, xp, level)
    _skill_levels[skill_id] = level
    _build.set_skill_levels(_skill_levels)

# --- handshake ----------------------------------------------------------------

func _on_auth_required(version: int) -> void:
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
    _player.activate()
    _net.send_craft_list() # the recipe registry is static; pull it once per session

## A starter plot was (re-)assigned: remember its id (for rent.pay/autopay),
## draw its outline/beacon in the world, feed the HUD compass, and — only on
## the very first grant — flash the onboarding banner (#11).
func _on_plot_assigned(plot_id: String, district: String, bounds: Dictionary, _tier: int, just_claimed: bool) -> void:
    _plot_id = plot_id
    _world.show_home_plot(bounds)
    var cx := float(bounds.get("x", 0)) + float(bounds.get("w", 0)) * 0.5
    var cy := float(bounds.get("y", 0)) + float(bounds.get("h", 0)) * 0.5
    _hud.set_home(cx, cy)
    if just_claimed:
        _hud.flash_announce("Your home plot is ready, in the %s!" % district.capitalize())

## Gather the nearest in-range resource node (resolved from the entity manager).

## Feed a `rent.status` push (login, pay, auto-pay toggle, or a ticker-driven
## change) to both the HUD's compact hint and the toggleable rent panel (#14).
func _on_rent_status(plot_id: String, due_at: int, paid_through: int, state: String, auto_pay: bool, gold: int) -> void:
    _hud.set_rent_hint(state, due_at)
    _rent.set_status(plot_id, due_at, paid_through, state, auto_pay, gold)

func _on_gather_pressed() -> void:
    var node_id := _entities.nearest_resource(_player.world_pos(), Protocol.GATHER_RANGE)
    if node_id != "":
        _net.send_gather_start(node_id)

func _on_status_update(id: String, zone: String, state: Dictionary) -> void:
    if id == _my_id:
        if not _seeded_position:
            # First authoritative snapshot: place us exactly where the server spawned us.
            _seeded_position = true
            _player.activate(Vector2(float(state.get("x", 600)), float(state.get("y", 600))))
        else:
            _player.reconcile(state)
        if zone != "":
            _hud.set_zone(zone)
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
