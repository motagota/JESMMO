## Login / register / guest overlay, built entirely in code (no .tscn to hand-edit).
##
## Shown on `auth_required`, hidden on `welcome`, and re-shown with a message on
## `auth_error`. Emits one signal per action; `Main` wires them to `NetworkClient`.
class_name Login
extends CanvasLayer

signal do_login(email: String, password: String)
signal do_register(email: String, password: String, character_name: String)
signal do_guest

var _email: LineEdit
var _password: LineEdit
var _name: LineEdit
var _message: Label
var _version: Label

func _ready() -> void:
	layer = 10
	var dim := ColorRect.new()
	dim.color = Color(0, 0, 0, 0.85)
	dim.set_anchors_preset(Control.PRESET_FULL_RECT)
	add_child(dim)

	var center := CenterContainer.new()
	center.set_anchors_preset(Control.PRESET_FULL_RECT)
	add_child(center)

	var box := VBoxContainer.new()
	box.custom_minimum_size = Vector2(340, 0)
	center.add_child(box)

	var title := Label.new()
	title.text = "Enter the Capital"
	title.add_theme_font_size_override("font_size", 24)
	box.add_child(title)

	_email = _add_field(box, "email")
	_password = _add_field(box, "password")
	_password.secret = true
	_name = _add_field(box, "character name (register only)")

	var row := HBoxContainer.new()
	box.add_child(row)
	_add_button(row, "Login", _on_login)
	_add_button(row, "Register", _on_register)
	_add_button(row, "Guest", func(): do_guest.emit())

	_message = Label.new()
	_message.modulate = Color(1.0, 0.42, 0.42)
	box.add_child(_message)

	_version = Label.new()
	_version.modulate = Color(0.4, 0.8, 0.5)
	_version.text = "protocol —"
	box.add_child(_version)

func _add_field(parent: Control, placeholder: String) -> LineEdit:
	var le := LineEdit.new()
	le.placeholder_text = placeholder
	le.custom_minimum_size = Vector2(0, 32)
	parent.add_child(le)
	return le

func _add_button(parent: Control, text: String, cb: Callable) -> void:
	var b := Button.new()
	b.text = text
	b.pressed.connect(cb)
	parent.add_child(b)

func _on_login() -> void:
	_message.text = ""
	do_login.emit(_email.text.strip_edges(), _password.text)

func _on_register() -> void:
	_message.text = ""
	do_register.emit(_email.text.strip_edges(), _password.text, _name.text.strip_edges())

# --- driven by Main -----------------------------------------------------------

func show_overlay(show: bool) -> void:
	visible = show

func set_error(text: String) -> void:
	_message.text = text
	visible = true

func set_version(v: int) -> void:
	_version.text = "protocol v%d" % v

func prefill_email(email: String) -> void:
	if email != "":
		_email.text = email
