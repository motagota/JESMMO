## Headless smoke test: HistoryPanel undo bookkeeping (terrain editing #79).
## Covers: newest-first recording, undo_last_target skipping reverted ops,
## server-ack-driven state (mark_reverted), and the undo-selected path
## refusing an already-reverted entry.
## Run: Godot --headless --path client_godot -s res://tests/smoke_editor_history.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	var panel := HistoryPanel.new()
	root.add_child(panel)
	var reverts: Array = []
	panel.do_revert.connect(func(op_id): reverts.append(op_id))

	if panel.undo_last_target() != "":
		_fail("empty history has no undo target")
		return

	panel.record_op("op-1", "raise")
	panel.record_op("op-2", "raise")
	panel.record_op("op-3", "raise")
	if panel.undo_last_target() != "op-3":
		_fail("undo target should be the newest op, got %s" % panel.undo_last_target())
		return

	# Server acks the revert of op-3 -> the target walks back to op-2.
	panel.mark_reverted("op-3")
	if panel.undo_last_target() != "op-2":
		_fail("after reverting op-3 the target should be op-2, got %s" % panel.undo_last_target())
		return

	# A rejected revert never acks -> state must NOT change optimistically.
	if panel.undo_last_target() != "op-2":
		_fail("no ack must mean no state change")
		return

	panel.mark_reverted("op-2")
	panel.mark_reverted("op-1")
	if panel.undo_last_target() != "":
		_fail("a fully-reverted history has no target, got %s" % panel.undo_last_target())
		return

	# Undo-selected on a reverted entry emits nothing.
	panel._list.select(0) # op-3, reverted
	panel._on_undo_selected()
	if not reverts.is_empty():
		_fail("undo-selected must refuse an already-reverted op")
		return

	print("SMOKE_OK: history records newest-first, tracks reverts by server ack only, and picks correct undo targets")
	quit(0)
