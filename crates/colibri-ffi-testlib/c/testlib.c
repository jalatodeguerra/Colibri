/* The C side of the FFI tests: scalars, strings, pointers and structs by value, with layouts
   chosen to exercise the platform ABI's classification (all-integer, all-float, a mix of both,
   padding, nesting, and structs too large for registers). */
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#define EXPORT __declspec(dllexport)
#else
#define EXPORT __attribute__((visibility("default")))
#endif

/* ---- scalars ---- */

EXPORT int32_t ct_add_i32(int32_t a, int32_t b) { return a + b; }
EXPORT int8_t ct_neg_i8(int8_t a) { return (int8_t)-a; }
EXPORT uint8_t ct_inc_u8(uint8_t a) { return (uint8_t)(a + 1); }
EXPORT int16_t ct_mul_i16(int16_t a, int16_t b) { return (int16_t)(a * b); }
EXPORT uint16_t ct_not_u16(uint16_t a) { return (uint16_t)~a; }
EXPORT uint32_t ct_mul_u32(uint32_t a, uint32_t b) { return a * b; }
EXPORT int64_t ct_add_i64(int64_t a, int64_t b) { return a + b; }
EXPORT uint64_t ct_u64_max(void) { return UINT64_MAX; }
EXPORT uint64_t ct_u64_half(uint64_t a) { return a / 2; }
EXPORT double ct_hyp(double a, double b) { return a * a + b * b; }
EXPORT float ct_scale_f32(float x, float k) { return x * k; }
EXPORT bool ct_is_even(int32_t a) { return a % 2 == 0; }
EXPORT bool ct_and(bool a, bool b) { return a && b; }
EXPORT void ct_nothing(void) {}
/* Many arguments of mixed classes: some go in registers, some on the stack. */
EXPORT double ct_many(int32_t a, double b, int32_t c, double d, int32_t e, double f, int32_t g,
                      double h, int32_t i, double j, int32_t k, double l, int32_t m, double n,
                      int32_t o, double p, int32_t q, double r) {
  return a + b + c + d + e + f + g + h + i + j + k + l + m + n + o + p + q + r;
}

/* ---- strings ---- */

EXPORT uint64_t ct_strlen(const char *s) { return (uint64_t)strlen(s); }
EXPORT const char *ct_greeting(void) { return "hello from C"; }
EXPORT const char *ct_null_string(void) { return NULL; }
EXPORT const char *ct_bad_utf8(void) { return "\xff\xfe"; }
EXPORT const char *ct_or_default(const char *s) { return s ? s : "(default)"; }
EXPORT const char *ct_join(const char *a, const char *b) {
  static char buf[256];
  size_t la = strlen(a), lb = strlen(b);
  if (la + lb + 2 > sizeof buf) return NULL;
  memcpy(buf, a, la);
  buf[la] = '-';
  memcpy(buf + la + 1, b, lb + 1);
  return buf;
}

/* ---- pointers ---- */

EXPORT void ct_set_i32(int32_t *p, int32_t v) { *p = v; }
EXPORT int32_t ct_sum_i32(const int32_t *p, int64_t n) {
  int32_t s = 0;
  for (int64_t i = 0; i < n; i++) s += p[i];
  return s;
}
EXPORT void ct_fill_f64(double *p, int64_t n, double v) {
  for (int64_t i = 0; i < n; i++) p[i] = v + (double)i;
}
EXPORT void ct_divmod(int32_t a, int32_t b, int32_t *quot, int32_t *rem) {
  *quot = a / b;
  *rem = a % b;
}
EXPORT int32_t *ct_null_ptr(void) { return NULL; }
EXPORT int32_t ct_is_null(const void *p) { return p == NULL; }

typedef struct Counter {
  int32_t value;
} Counter;
EXPORT Counter *ct_counter_new(int32_t start) {
  Counter *c = malloc(sizeof *c);
  c->value = start;
  return c;
}
EXPORT void ct_counter_inc(Counter *c) { c->value++; }
EXPORT int32_t ct_counter_get(const Counter *c) { return c->value; }
EXPORT void ct_counter_free(Counter *c) { free(c); }

/* ---- structs ---- */

typedef struct { int32_t x, y; } Point;               /* 8 bytes, one INTEGER eightbyte */
typedef struct { double x, y; } Vec2d;                /* 16 bytes, two SSE eightbytes */
typedef struct { float x, y; } Vec2f;                 /* 8 bytes, two floats in one SSE eightbyte */
typedef struct { float x, y, z; } Vec3f;              /* 12 bytes */
typedef struct { uint8_t r, g, b, a; } Color;         /* 4 bytes */
typedef struct { int32_t id; double score; } Mixed;   /* INTEGER + SSE, with padding */
typedef struct { double w; int32_t n; float f; uint8_t tag; } Mixed2; /* SSE + (INTEGER, SSE) */
typedef struct { int64_t a, b, c, d; } Big;           /* 32 bytes: passed in memory */
typedef struct { Point min, max; } Rect;              /* nested */
typedef struct { uint8_t tag; int64_t value; bool flag; uint16_t n; } Padded;
typedef struct { const char *name; int32_t len; } Named;
typedef struct { int8_t a; int16_t b; int32_t c; int64_t d; uint8_t e; } Ladder;

EXPORT Point ct_point_add(Point a, Point b) { return (Point){a.x + b.x, a.y + b.y}; }
EXPORT int32_t ct_point_dot(Point a, Point b) { return a.x * b.x + a.y * b.y; }
EXPORT void ct_point_scale(Point *p, int32_t k) {
  p->x *= k;
  p->y *= k;
}
EXPORT Vec2d ct_vec2d_scale(Vec2d v, double k) { return (Vec2d){v.x * k, v.y * k}; }
EXPORT double ct_vec2d_len2(Vec2d v) { return v.x * v.x + v.y * v.y; }
EXPORT Vec2f ct_vec2f_add(Vec2f a, Vec2f b) { return (Vec2f){a.x + b.x, a.y + b.y}; }
EXPORT Vec3f ct_vec3f_cross(Vec3f a, Vec3f b) {
  return (Vec3f){a.y * b.z - a.z * b.y, a.z * b.x - a.x * b.z, a.x * b.y - a.y * b.x};
}
EXPORT Color ct_color_invert(Color c) { return (Color){255 - c.r, 255 - c.g, 255 - c.b, c.a}; }
EXPORT uint32_t ct_color_pack(Color c) {
  return ((uint32_t)c.r << 24) | ((uint32_t)c.g << 16) | ((uint32_t)c.b << 8) | c.a;
}
EXPORT Mixed ct_mixed_make(int32_t id, double score) { return (Mixed){id, score}; }
EXPORT double ct_mixed_sum(Mixed m) { return m.id + m.score; }
EXPORT Mixed2 ct_mixed2_make(double w, int32_t n, float f, uint8_t tag) {
  return (Mixed2){w, n, f, tag};
}
EXPORT double ct_mixed2_sum(Mixed2 m) { return m.w + m.n + m.f + m.tag; }
EXPORT Big ct_big_make(int64_t s) { return (Big){s, s * 2, s * 3, s * 4}; }
EXPORT int64_t ct_big_sum(Big b) { return b.a + b.b + b.c + b.d; }
EXPORT Big ct_big_add(Big a, Big b) {
  return (Big){a.a + b.a, a.b + b.b, a.c + b.c, a.d + b.d};
}
EXPORT Rect ct_rect_union(Rect a, Rect b) {
  Rect r;
  r.min.x = a.min.x < b.min.x ? a.min.x : b.min.x;
  r.min.y = a.min.y < b.min.y ? a.min.y : b.min.y;
  r.max.x = a.max.x > b.max.x ? a.max.x : b.max.x;
  r.max.y = a.max.y > b.max.y ? a.max.y : b.max.y;
  return r;
}
EXPORT int32_t ct_rect_area(Rect r) { return (r.max.x - r.min.x) * (r.max.y - r.min.y); }
EXPORT Padded ct_padded_make(uint8_t tag, int64_t value, bool flag, uint16_t n) {
  return (Padded){tag, value, flag, n};
}
EXPORT int64_t ct_padded_sum(Padded p) { return p.tag + p.value + (p.flag ? 1000 : 0) + p.n; }
EXPORT Named ct_named_make(const char *name) { return (Named){name, (int32_t)strlen(name)}; }
EXPORT int32_t ct_named_len(Named n) { return n.len; }
EXPORT Ladder ct_ladder_make(void) { return (Ladder){-1, -2, -3, -4, 5}; }
EXPORT int64_t ct_ladder_sum(Ladder l) { return l.a + l.b + l.c + l.d + l.e; }
EXPORT Point ct_point_from_ptr(const Point *p) { return *p; }

/* ---- layout, as the C compiler sees it ---- */

#define LAYOUT(T, F) if (!strcmp(type, #T) && field && !strcmp(field, #F)) return offsetof(T, F)
EXPORT uint64_t ct_layout(const char *type, const char *field) {
  if (field && !strcmp(field, "#align")) {
#define ALIGN(T) if (!strcmp(type, #T)) return _Alignof(T)
    ALIGN(Point); ALIGN(Vec2d); ALIGN(Vec2f); ALIGN(Vec3f); ALIGN(Color); ALIGN(Mixed);
    ALIGN(Mixed2); ALIGN(Big); ALIGN(Rect); ALIGN(Padded); ALIGN(Named); ALIGN(Ladder);
    return 0;
  }
  if (!field) {
#define SIZE(T) if (!strcmp(type, #T)) return sizeof(T)
    SIZE(Point); SIZE(Vec2d); SIZE(Vec2f); SIZE(Vec3f); SIZE(Color); SIZE(Mixed);
    SIZE(Mixed2); SIZE(Big); SIZE(Rect); SIZE(Padded); SIZE(Named); SIZE(Ladder);
    return 0;
  }
  LAYOUT(Point, y); LAYOUT(Vec2d, y); LAYOUT(Vec2f, y);
  LAYOUT(Vec3f, y); LAYOUT(Vec3f, z);
  LAYOUT(Color, g); LAYOUT(Color, b); LAYOUT(Color, a);
  LAYOUT(Mixed, score);
  LAYOUT(Mixed2, n); LAYOUT(Mixed2, f); LAYOUT(Mixed2, tag);
  LAYOUT(Big, b); LAYOUT(Big, c); LAYOUT(Big, d);
  LAYOUT(Rect, max);
  LAYOUT(Padded, value); LAYOUT(Padded, flag); LAYOUT(Padded, n);
  LAYOUT(Named, len);
  LAYOUT(Ladder, b); LAYOUT(Ladder, c); LAYOUT(Ladder, d); LAYOUT(Ladder, e);
  return 0;
}
