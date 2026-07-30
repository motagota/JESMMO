## OHLCV price chart (market epic #136, issue #143): candles with volume bars
## underneath, drawn directly.
##
## Custom `_draw` rather than a charting library — the client has no external
## dependencies anywhere else, and OHLCV is a handful of rectangles and lines.
##
## A GAP IS A GAP. The server omits buckets with no trades (carrying the last
## price forward would invent a price nobody paid), and this honours that: a
## quiet hour leaves empty space on the x axis rather than a flat candle. That's
## what lets a player see "nothing traded overnight" instead of a false floor.
class_name PriceChart
extends Control

## Candles as the server sent them: `{t, o, h, l, c, v, n}`, oldest first.
var _candles: Array = []
var _interval := 3600
var _item := ""

const _UP := Color(0.45, 0.85, 0.5)
const _DOWN := Color(0.9, 0.42, 0.42)
const _AXIS := Color(0.45, 0.45, 0.52)
const _LABEL := Color(0.75, 0.78, 0.85)
const _VOLUME := Color(0.45, 0.55, 0.75, 0.55)
## Share of the height given to the volume strip at the bottom.
const _VOLUME_FRACTION := 0.24
const _PAD_LEFT := 34.0
const _PAD_BOTTOM := 14.0
const _PAD_TOP := 6.0

func set_history(item_id: String, interval_secs: int, candles: Array) -> void:
	_item = item_id
	_interval = maxi(interval_secs, 1)
	_candles = candles
	queue_redraw()

func _draw() -> void:
	var w := size.x
	var h := size.y
	draw_rect(Rect2(Vector2.ZERO, size), Color(0.09, 0.10, 0.13))

	if _candles.is_empty():
		var msg := "no trades yet" if _item == "" else "no trades in %s yet" % _item
		draw_string(ThemeDB.fallback_font, Vector2(_PAD_LEFT, h * 0.5), msg,
			HORIZONTAL_ALIGNMENT_LEFT, -1, 11, Color(0.55, 0.55, 0.6))
		return

	# Price range across the window, padded so the extremes aren't on the edge.
	var lo := INF
	var hi := -INF
	var max_vol := 1
	for c_v in _candles:
		var c: Dictionary = c_v
		lo = minf(lo, float(c.get("l", 0)))
		hi = maxf(hi, float(c.get("h", 0)))
		max_vol = maxi(max_vol, int(c.get("v", 0)))
	if hi <= lo:
		# A single traded price: give it room so the candle isn't a hairline.
		hi = lo + 1.0
	var span := hi - lo
	lo -= span * 0.08
	hi += span * 0.08
	span = hi - lo

	var plot_h := (h - _PAD_TOP - _PAD_BOTTOM) * (1.0 - _VOLUME_FRACTION)
	var vol_top := _PAD_TOP + plot_h + 4.0
	var vol_h := (h - _PAD_TOP - _PAD_BOTTOM) * _VOLUME_FRACTION
	var plot_w := w - _PAD_LEFT - 4.0

	# Axes, plus the price extremes as labels — a chart without numbers on it
	# tells you the shape but not whether it matters.
	draw_line(Vector2(_PAD_LEFT, _PAD_TOP), Vector2(_PAD_LEFT, _PAD_TOP + plot_h), _AXIS)
	draw_line(Vector2(_PAD_LEFT, _PAD_TOP + plot_h), Vector2(w - 4.0, _PAD_TOP + plot_h), _AXIS)
	draw_string(ThemeDB.fallback_font, Vector2(2, _PAD_TOP + 8), "%dg" % int(hi),
		HORIZONTAL_ALIGNMENT_LEFT, -1, 9, _LABEL)
	draw_string(ThemeDB.fallback_font, Vector2(2, _PAD_TOP + plot_h), "%dg" % int(lo),
		HORIZONTAL_ALIGNMENT_LEFT, -1, 9, _LABEL)

	# Time axis spans the whole window, so an absent bucket leaves a real gap
	# rather than the candles closing up as if it never existed.
	var first_t := int((_candles[0] as Dictionary).get("t", 0))
	var last_t := int((_candles[_candles.size() - 1] as Dictionary).get("t", 0))
	var buckets := maxi((last_t - first_t) / _interval + 1, 1)
	var slot := plot_w / float(buckets)
	var body := maxf(minf(slot * 0.62, 12.0), 1.0)

	for c_v in _candles:
		var c: Dictionary = c_v
		var idx := (int(c.get("t", 0)) - first_t) / _interval
		var cx := _PAD_LEFT + slot * (float(idx) + 0.5)
		var o := float(c.get("o", 0))
		var cl := float(c.get("c", 0))
		var hgh := float(c.get("h", 0))
		var low := float(c.get("l", 0))
		var col := _UP if cl >= o else _DOWN

		# Wick, then body. A doji (open == close) still gets a visible line.
		draw_line(Vector2(cx, _y(hgh, lo, span, plot_h)), Vector2(cx, _y(low, lo, span, plot_h)), col)
		var y1 := _y(maxf(o, cl), lo, span, plot_h)
		var y2 := _y(minf(o, cl), lo, span, plot_h)
		draw_rect(Rect2(cx - body * 0.5, y1, body, maxf(y2 - y1, 1.0)), col)

		# Volume underneath, on its own scale.
		var vh := vol_h * (float(int(c.get("v", 0))) / float(max_vol))
		draw_rect(Rect2(cx - body * 0.5, vol_top + vol_h - vh, body, maxf(vh, 1.0)), _VOLUME)

	# Latest close, which is the number a trader actually wants.
	var last: Dictionary = _candles[_candles.size() - 1]
	draw_string(ThemeDB.fallback_font, Vector2(_PAD_LEFT + 2, _PAD_TOP + 9),
		"%s  last %dg  vol %d" % [_item, int(last.get("c", 0)), int(last.get("v", 0))],
		HORIZONTAL_ALIGNMENT_LEFT, -1, 10, _LABEL)

func _y(price: float, lo: float, span: float, plot_h: float) -> float:
	return _PAD_TOP + plot_h - ((price - lo) / span) * plot_h
