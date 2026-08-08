## Plot management (business epic #179, issue #185): access policy, the roster,
## and the recovery vault.
##
## Everything the capital layer added server-side across #181-#184 had no UI at
## all. This is that UI, and it sits beside `RentPanel` rather than inside it:
## rent is a recurring obligation you check, this is a business you configure.
##
## THE PANEL IS A VIEW, NEVER A PERMISSION. Every button round-trips; the server
## re-checks ownership, lease state and range on each. That is the rule every
## panel in this project follows (#137, #167) and the one most easily broken by
## a client that "knows" the player is the owner. The only thing decided here is
## what to draw.
##
## Note what is deliberately ABSENT: a lease board. #185 called for one — vacant
## plots, price, deposit, confirm — but plots are AUTO-ASSIGNED when a player
## enters a district (`claim_plot` at the district gate), there is no landlord
## NPC, and no leasing transaction to put on a board. Building a board for a
## transaction that does not exist would be inventing a feature to justify a
## screen.
class_name PlotPanel
extends CanvasLayer

## Policy, roster and vault actions, all of which the server re-validates.
signal do_set_policy(mode: String, fee_gp: int, skill_floor: int, in_kind: float, in_kind_max: int)
signal do_grant(character_id: String, role: String, days: int)
signal do_revoke(character_id: String)
signal do_claim_vault()
signal do_refresh()

var _title: Label
var _state_line: Label
var _body: VBoxContainer
var _note: Label

var _plot_id := ""
var _lease_state := "active"
var _mode := "closed"
var _fee_gp := 0
var _skill_floor := 0
var _in_kind := 0.0
var _in_kind_max := 0
var _roster: Array = []
var _vault: Array = []
var _note_text := ""

const MODES := ["closed", "public", "fee", "roster"]


func _ready() -> void:
	layer = 6
	var panel := PanelContainer.new()
	panel.set_anchors_preset(Control.PRESET_CENTER_LEFT)
	panel.position = Vector2(24, -240)
	panel.custom_minimum_size = Vector2(420, 480)
	add_child(panel)

	var root := VBoxContainer.new()
	root.add_theme_constant_override("separation", 6)
	panel.add_child(root)

	_title = Label.new()
	_title.text = "Your plot"
	_title.add_theme_font_size_override("font_size", 20)
	root.add_child(_title)

	_state_line = Label.new()
	_state_line.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	_state_line.custom_minimum_size = Vector2(400, 0)
	root.add_child(_state_line)

	_note = Label.new()
	_note.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	_note.custom_minimum_size = Vector2(400, 0)
	root.add_child(_note)

	root.add_child(HSeparator.new())

	_body = VBoxContainer.new()
	_body.add_theme_constant_override("separation", 4)
	root.add_child(_body)

	visible = false


func set_lease(plot_id: String, state: String) -> void:
	_plot_id = plot_id
	_lease_state = state
	_redraw()


## Just the state, from `plot.roster`, which knows it without knowing which
## panel is open.
func set_lease_state(state: String) -> void:
	_lease_state = state
	_redraw()


func set_policy(mode: String, fee_gp: int, skill_floor: int, in_kind: float, in_kind_max: int) -> void:
	_mode = mode
	_fee_gp = fee_gp
	_skill_floor = skill_floor
	_in_kind = in_kind
	_in_kind_max = in_kind_max
	_redraw()


func set_roster(grants: Array) -> void:
	_roster = grants
	_redraw()


func set_vault(entries: Array) -> void:
	_vault = entries
	_redraw()


func note(text: String) -> void:
	_note_text = text
	if _note != null:
		_note.text = text


func show_panel(on: bool) -> void:
	visible = on
	if on:
		do_refresh.emit()


func _redraw() -> void:
	if _title == null:
		return

	# The lease state, and it has to be impossible to miss. A colour change is
	# not enough for something that ends in losing the plot (#184).
	match _lease_state:
		"lapsed":
			_state_line.text = "⚠ RENT OVERDUE — your stations still run, but their fees go to the landlord until you pay."
			_state_line.modulate = Color(1.0, 0.8, 0.35)
		"derelict":
			_state_line.text = "⚠⚠ DERELICT — production has STOPPED. Pay now or the plot returns to the pool and everything on it goes to your recovery vault."
			_state_line.modulate = Color(1.0, 0.45, 0.35)
		_:
			_state_line.text = "Lease in good standing."
			_state_line.modulate = Color(0.75, 0.85, 0.75)
	_note.text = _note_text

	for c in _body.get_children():
		_body.remove_child(c)
		c.queue_free()

	_build_policy()
	_body.add_child(HSeparator.new())
	_build_roster()
	# The vault only appears when there is something in it. An empty one is a
	# reminder of nothing.
	if not _vault.is_empty():
		_body.add_child(HSeparator.new())
		_build_vault()


func _build_policy() -> void:
	var head := Label.new()
	head.text = "Who may use your stations"
	_body.add_child(head)

	var row := HBoxContainer.new()
	for m in MODES:
		var mode := String(m)
		var b := Button.new()
		b.text = mode
		b.disabled = mode == _mode
		b.pressed.connect(func():
			do_set_policy.emit(mode, _fee_gp, _skill_floor, _in_kind, _in_kind_max))
		row.add_child(b)
	_body.add_child(row)

	var detail := Label.new()
	detail.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	detail.custom_minimum_size = Vector2(400, 0)
	match _mode:
		"closed": detail.text = "  Closed — only you."
		"public": detail.text = "  Public — anyone, free."
		"fee": detail.text = "  Fee — anyone, %dg per job." % _fee_gp
		"roster": detail.text = "  Roster — only people you have listed below."
		_: detail.text = "  " + _mode
	# The in-kind share reads as "1 in N", never a decimal — the whole reason it
	# was chosen over a percentage is that it needs no explanation (#182).
	if _in_kind > 0.0:
		var one_in := int(round(1.0 / _in_kind))
		detail.text += "  You also take 1 in %d of what is made" % one_in
		if _in_kind_max > 0:
			detail.text += ", up to %dg" % _in_kind_max
		detail.text += "."
	if _skill_floor > 0:
		detail.text += "  Minimum skill %d." % _skill_floor
	_body.add_child(detail)


func _build_roster() -> void:
	var head := Label.new()
	head.text = "Roster (%d)" % _roster.size()
	_body.add_child(head)

	if _roster.is_empty():
		var none := Label.new()
		none.text = "  Nobody. Grants let others work here; every one expires."
		_body.add_child(none)
		return

	for g in _roster:
		var row := HBoxContainer.new()
		var who := String(g.get("character_id", "")).substr(0, 8)
		var label := Label.new()
		label.custom_minimum_size = Vector2(300, 0)
		# EXPIRY IS SHOWN, not just stored. An expiry nobody can see is an
		# expiry that surprises people (#183).
		if bool(g.get("expired", false)):
			label.text = "  %s — %s (EXPIRED)" % [who, String(g.get("role", ""))]
			label.modulate = Color(1, 1, 1, 0.5)
		else:
			label.text = "  %s — %s, %d days left" % [
				who, String(g.get("role", "")), int(g.get("days_left", 0))]
		row.add_child(label)

		var b := Button.new()
		b.text = "Remove"
		var cid := String(g.get("character_id", ""))
		b.pressed.connect(func(): do_revoke.emit(cid))
		row.add_child(b)
		_body.add_child(row)


func _build_vault() -> void:
	var head := Label.new()
	head.text = "⚑ Recovery vault"
	head.modulate = Color(1.0, 0.85, 0.4)
	_body.add_child(head)

	var soonest := 99999
	for v in _vault:
		var row := Label.new()
		var days := int(v.get("days_left", 0))
		soonest = min(soonest, days)
		row.text = "  %s x%d (from your %s) — %d days left" % [
			String(v.get("item", "")), int(v.get("qty", 0)),
			String(v.get("source", "")), days]
		_body.add_child(row)

	var warn := Label.new()
	warn.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	warn.custom_minimum_size = Vector2(400, 0)
	# Said plainly and repeatedly, because this is the one place the game DOES
	# eventually destroy property — and only after telling you it will.
	warn.text = "These are the contents of a plot you lost. Claim them within %d days or they are gone." % soonest
	warn.modulate = Color(1.0, 0.8, 0.5)
	_body.add_child(warn)

	var b := Button.new()
	b.text = "Claim everything"
	b.pressed.connect(func(): do_claim_vault.emit())
	_body.add_child(b)
