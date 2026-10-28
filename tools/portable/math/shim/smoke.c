#include "pn_codec_math.h"

/* ABI/link check only; raw-bit parity is tested by codec_oracle.rs. */
int main(void) {
    int exponent = 0;
    if (pn_codec_frexp(5.0, &exponent) != 0.625 || exponent != 3) return 1;
    if (pn_codec_lround(-2.5) != -3) return 2;
    if (pn_codec_pow(2.0, 3.0) != 8.0) return 3;
    if (pn_codec_cosf(0.0f) != 1.0f) return 4;
    return 0;
}
