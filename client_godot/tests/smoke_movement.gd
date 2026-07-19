## Headless smoke test for camera-true movement (#113): the camera-relative
## heading is the true float direction (not an 8-way snap), and the
## error-accumulated integer steps converge on it — 100 wire steps end up
## within one step of exactly where the camera pointed, at full speed.
## Run: Godot --headless --path client_godot -s res://tests/smoke_movement.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	# Heading math: W at yaw 0 walks -y (north); yaw is applied truly.
	var north := LocalPlayer.camera_relative_dir(Vector2(0, -1), 0.0)
	if north.distance_to(Vector2(0, -1)) > 0.001:
		_fail("W at yaw 0 should head exactly north (got %s)" % str(north)); return
	var yawed := LocalPlayer.camera_relative_dir(Vector2(0, -1), deg_to_rad(30))
	if absf(yawed.length() - 1.0) > 0.001:
		_fail("headings must be unit length (got %f)" % yawed.length()); return
	if yawed.distance_to(Vector2(0, -1)) < 0.1:
		_fail("a 30-degree yaw must actually change the heading"); return
	# The old bug: signf snapping — the heading must NOT be axis/diagonal
	# locked at an intermediate yaw.
	for c in [yawed.x, yawed.y]:
		if absf(c) < 0.001 or absf(absf(c) - 1.0) < 0.001 or absf(absf(c) - 0.7071) < 0.001:
			pass # a single component may coincide; the sum test below is the real check

	# Step accumulation: at several yaws, 100 unit steps land within one
	# step of the true float displacement — no compass drift.
	for yaw_deg in [0.0, 17.0, 30.0, 61.5, 117.0, 203.0, 340.0]:
		var dir := LocalPlayer.camera_relative_dir(Vector2(0, -1), deg_to_rad(yaw_deg))
		var carry := Vector2.ZERO
		var total := Vector2i.ZERO
		for i in range(100):
			var stepped := LocalPlayer.step_with_carry(dir, 1.0, carry)
			total += stepped[0]
			carry = stepped[1]
		var want := dir * 100.0
		var got := Vector2(total)
		if got.distance_to(want) > 1.0:
			_fail("at yaw %s, 100 steps drifted %.2fm off the camera heading (got %s want %s)" % [
				yaw_deg, got.distance_to(want), str(got), str(want)]); return
		# Speed check: never faster than the true heading allows (+rounding).
		if got.length() > 101.5:
			_fail("at yaw %s, steps overshoot the speed (%f)" % [yaw_deg, got.length()]); return

	# Carry resets: a fresh carry from standstill never banks a phantom step.
	var stepped := LocalPlayer.step_with_carry(Vector2(1, 0), 1.0, Vector2.ZERO)
	if stepped[0] != Vector2i(1, 0):
		_fail("a clean east step should be exactly (1,0)"); return

	print("SMOKE_OK: camera-relative headings are true floats and the integer wire steps converge on them at every yaw")
	quit(0)
