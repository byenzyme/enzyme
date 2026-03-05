// Compatibility shims for glibc 2.38+ C23 symbols.
//
// The pre-built ONNX Runtime static libraries (from pyke) were compiled
// with glibc 2.38+ headers, which redirect strtoll/strtol to their
// __isoc23_* variants. When linking on older glibc (e.g. 2.35 on
// Ubuntu 22.04), these symbols are missing. These shims provide them
// by delegating to the standard functions.

#include <stdlib.h>

long long __isoc23_strtoll(const char *nptr, char **endptr, int base) {
    return strtoll(nptr, endptr, base);
}

unsigned long long __isoc23_strtoull(const char *nptr, char **endptr, int base) {
    return strtoull(nptr, endptr, base);
}

long __isoc23_strtol(const char *nptr, char **endptr, int base) {
    return strtol(nptr, endptr, base);
}
