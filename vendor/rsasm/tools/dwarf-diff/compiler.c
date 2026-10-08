#include <stdarg.h>
struct S { int a; long b; };
extern int g(int);
static int helper(int x) { return x * 3 + g(x); }
int sum(int n, ...) {
  va_list ap; int s = 0;
  va_start(ap, n);
  for (int i = 0; i < n; i++) s += va_arg(ap, int);
  va_end(ap);
  return s + helper(s);
}
long big(struct S *p, int k) {
  char buf[256];
  for (int i = 0; i < 256; i++) buf[i] = (char)(i ^ k);
  if (k > 10) return p->b + buf[k & 0xff];
  return p->a + g(buf[3]);
}
void leaf(void) {}
