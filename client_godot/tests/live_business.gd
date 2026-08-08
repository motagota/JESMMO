## The business layer, end to end against running servers.
##
## Two owners, a furnace, a roster and a fee paid in goods. It exists because
## #182 and #183 both SHIPPED without this verification: the only way to put a
## character into a known state was editing the database directly, and that does
## not survive the gateway's cache for a logged-in character.
##
## #195 fixed that with admin-socket seeding, and this is the payoff — kept as a
## permanent test rather than a throwaway probe, because the two things it
## covers are exactly the ones that went unchecked:
##
##   #183  the roster gate: stranger refused, worker admitted, revoked refused
##   #182  the in-kind share taken in GOODS at collect
##
## Needs running servers. Run:
##   Godot --headless --path client_godot -s res://tests/live_business.gd
##
## KNOWN FLAKE: two runs back to back can fail at login, because the gateway
## still holds the previous run's sessions and a duplicate login collapses onto
## the old one. Leave a few seconds between runs. Worth fixing properly if this
## ever joins an automated sweep; recorded rather than papered over, because a
## flaky test you have been told to re-run is a test you stop believing.
extends SceneTree

const OWNER_EMAIL := "def_owner@t.test"
const USER_EMAIL := "def_user@t.test"

var _seed
var _owner
var _user
var _opid := ""
var _upid := ""
var _ocid := ""
var _ucid := ""
var _opos := Vector2.ZERO
var _upos := Vector2.ZERO
var _oplot: Dictionary = {}
var _target := Vector2.ZERO
var _ostations: Array = []
var _ustate: Dictionary = {}
var _collected: Dictionary = {}
var _failures: Array = []

func _mk(email: String, name: String):
	var n = load("res://net/NetworkClient.gd").new()
	root.add_child(n)
	n.auth_required.connect(func(_v): n.login(email, "pw12345"))
	n.auth_error.connect(func(_m): n.register(email, "pw12345", name))
	return n

func _initialize() -> void:
	_seed = ProbeSeed.new()
	root.add_child(_seed)
	_owner = _mk(OWNER_EMAIL, "Owner")
	_user = _mk(USER_EMAIL, "Customer")

	_owner.welcome.connect(func(d): _opid = String(d.get("player_id", "")))
	_owner.status_update.connect(func(id, _z, st):
		if id == _opid: _opos = Vector2(float(st.get("x", 0)), float(st.get("y", 0))))
	_owner.plot_assigned.connect(func(pid, _d, b, _t, _j): _oplot = {"id": pid, "bounds": b})
	_owner.station_list.connect(func(s): _ostations = s)
	_owner.build_error.connect(func(r): print("LIVE: owner build error -> %s" % r))

	_user.welcome.connect(func(d): _upid = String(d.get("player_id", "")))
	_user.status_update.connect(func(id, _z, st):
		if id == _upid: _upos = Vector2(float(st.get("x", 0)), float(st.get("y", 0))))
	_user.station_state.connect(func(s): _ustate = s)
	_user.station_collected.connect(func(_slot, failed, spoiled, _fr, _b, items):
		_collected = {"failed": failed, "spoiled": spoiled, "items": items})

	_owner.connect_to("ws://127.0.0.1:8766")
	_user.connect_to("ws://127.0.0.1:8766")
	_run()

func _wait(frames: int) -> void:
	for i in range(frames):
		await process_frame

func _walk(net, from_ref: Callable, to: Vector2, tries: int = 60) -> bool:
	for i in range(tries):
		var here: Vector2 = from_ref.call()
		if here.distance_to(to) <= 12.0:
			return true
		var d := to - here
		net.send_move(int(d.x), int(d.y))
		await _wait(40)
	return false

func _mine() -> Dictionary:
	for s in _ostations:
		if bool(s.get("mine", false)):
			return s
	return {}

func _run() -> void:
	# POLL, do not guess. A fixed wait passed on one machine and raced on the
	# next — and a flaky live test is worse than none, because it teaches you to
	# ignore the failures.
	for i in range(200):
		await _wait(10)
		if _opid != "" and _upid != "" and not _oplot.is_empty():
			break
	if _opid == "" or _upid == "" or _oplot.is_empty():
		print("LIVE_FAIL: never logged in or never got a plot (opid=%s upid=%s plot=%s)"
			% [_opid != "", _upid != "", not _oplot.is_empty()])
		quit(1); return

	if not await _seed.connect_admin():
		print("LIVE_FAIL: no admin socket"); quit(1); return
	# Registration is asynchronous, so the character row may lag the welcome.
	for i in range(60):
		_ocid = await _seed.whois(OWNER_EMAIL)
		_ucid = await _seed.whois(USER_EMAIL)
		if _ocid != "" and _ucid != "":
			break
		await _wait(20)
	if _ocid == "" or _ucid == "":
		print("LIVE_FAIL: whois could not resolve the probe accounts"); quit(1); return

	var b: Dictionary = _oplot.get("bounds", {})
	_target = Vector2(
		float(b.get("x", 0)) + float(b.get("w", 0)) * 0.5,
		float(b.get("y", 0)) + float(b.get("h", 0)) * 0.5)

	# --- setup, through the same paths a player uses -------------------------
	await _seed.items(_ocid, "stone", 40, true)
	await _seed.items(_ocid, "iron_ingot", 12, true)
	await _seed.items(_ucid, "iron_ore", 8)
	await _seed.items(_ucid, "charcoal", 4)
	await _seed.skill(_ucid, "smelting", 8)
	await _wait(60)

	# The owner has to STAND on their plot. The gateway validates the
	# coordinates against the plot bounds, but `build_place` only reaches it if
	# the ZONE saw the player on a plot in the first place — two gates, and an
	# earlier probe of mine only noticed the second one.
	if _mine().is_empty():
		if not await _walk(_owner, func(): return _opos, _target):
			print("LIVE_FAIL: the owner never reached their plot"); quit(1); return
		_owner.send_build_place("station", int(_target.x), int(_target.y), 0)
		await _wait(150)
	if _mine().is_empty():
		print("LIVE_FAIL: the owner has no station"); quit(1); return
	print("LIVE: owner's furnace is up")

	# ===================== #183: the roster gate ==========================
	_owner.send_station_policy("roster", 0, 0, 0.0, 0)
	await _wait(120)
	if not await _walk(_user, func(): return _upos, _target):
		print("LIVE_FAIL: the customer never reached the plot"); quit(1); return

	_user.send_station_open(); await _wait(120)
	if String(_ustate.get("access_error", "")) != "not_on_the_roster":
		_failures.append("#183 a stranger was NOT refused at a roster-only plot (got '%s')"
			% String(_ustate.get("access_error", "")))
	else:
		print("LIVE: #183 a stranger is refused")

	_owner.send_station_grant(_ucid, "worker", 30)
	await _wait(120)
	_user.send_station_open(); await _wait(120)
	if String(_ustate.get("access_error", "")) != "":
		_failures.append("#183 a rostered worker was refused: '%s'" % String(_ustate.get("access_error", "")))
	else:
		print("LIVE: #183 a rostered worker is let in")

	_owner.send_station_revoke(_ucid)
	await _wait(120)
	_user.send_station_open(); await _wait(120)
	if String(_ustate.get("access_error", "")) != "not_on_the_roster":
		_failures.append("#183 a revoked worker was still let in")
	else:
		print("LIVE: #183 a revoked worker is refused again")

	# ============ #182: the in-kind handover at collect ===================
	# One ingot in four, paid in goods rather than gold.
	_owner.send_station_policy("fee", 0, 0, 0.25, 0)
	await _wait(120)
	_user.send_station_open(); await _wait(120)
	var share := float(_ustate.get("owner_fee_in_kind", 0.0))
	if abs(share - 0.25) > 0.001:
		_failures.append("#182 the panel quoted a share of %f, not 0.25" % share)
	else:
		print("LIVE: #182 the panel quotes 1 in 4 before committing")

	if int(_ustate.get("fuel_units", 0)) < 8:
		_user.send_station_load_fuel("charcoal", 4)
		await _wait(120)
	_user.send_station_open(); await _wait(60)
	_user.send_station_start("iron_ingot_x4")
	await _wait(120)

	# The x4 job runs 48s at level 8; poll for it.
	for i in range(120):
		_user.send_station_open()
		await _wait(60)
		var jobs: Array = _ustate.get("jobs", [])
		if not jobs.is_empty() and String(jobs[0].get("state", "")) in ["ready", "spoiled"]:
			_user.send_station_collect(String(jobs[0].get("id", "")))
			await _wait(120)
			break

	if _collected.is_empty():
		_failures.append("#182 the job never finished")
	else:
		var got := 0
		for it in _collected.get("items", []):
			if String(it.get("item", "")) == "iron_ingot":
				got = int(it.get("qty", 0))
		if got != 3:
			_failures.append("#182 the customer got %d ingots from a x4 job, expected 3 after a 1-in-4 share" % got)
		else:
			print("LIVE: #182 a x4 smelt paid the customer 3 — the fourth went to the owner in goods")

	if _failures.is_empty():
		print("LIVE_OK: both deferred verifications pass live — the roster gate admits and refuses correctly, and the in-kind share is taken in goods at collect")
		quit(0)
	else:
		for f in _failures:
			print("LIVE_FAIL: %s" % f)
		quit(1)
