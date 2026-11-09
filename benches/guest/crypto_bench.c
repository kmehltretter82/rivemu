/*
 * crypto_bench.c: the kinds of loops that dominate the Debian boot of the
 * P17 benchmark (module-signature and X.509 checks): multi-limb multiply-
 * accumulate as in Linux's mpihelp_* and vli_*, SHA-256 and Keccak-f[1600],
 * compiled for rv64gc. start.S runs it in S-mode with Sv39 paging, like the
 * kernel, and powers off through the SiFive test device.
 */
#include <stdint.h>

typedef uint64_t u64;
typedef uint32_t u32;

#define LIMBS 32
/* Iterations; -DSCALE=10 divides them by 10 (for callgrind). */
#ifndef SCALE
#define SCALE 1
#endif

static u64 a[LIMBS], b[LIMBS], r[2 * LIMBS];

static u64 addmul_1(u64 *res, const u64 *s1, long n, u64 limb)
{
    u64 cy = 0;
    for (long i = 0; i < n; i++) {
        unsigned __int128 p = (unsigned __int128)s1[i] * limb;
        u64 lo = (u64)p + cy;
        u64 hi = (u64)(p >> 64) + (lo < cy);
        u64 t = res[i] + lo;
        hi += t < lo;
        res[i] = t;
        cy = hi;
    }
    return cy;
}

static void mul_n(u64 *res, const u64 *x, const u64 *y, long n)
{
    for (long i = 0; i < 2 * n; i++)
        res[i] = 0;
    for (long i = 0; i < n; i++)
        res[i + n] = addmul_1(res + i, x, n, y[i]);
}

static const u32 K256[64] = {
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
};

#define ROR32(x, n) (((x) >> (n)) | ((x) << (32 - (n))))

static void sha256_block(u32 *st, const unsigned char *p)
{
    u32 w[64];
    for (int i = 0; i < 16; i++)
        w[i] = (u32)p[4 * i] << 24 | (u32)p[4 * i + 1] << 16 | (u32)p[4 * i + 2] << 8 | p[4 * i + 3];
    for (int i = 16; i < 64; i++) {
        u32 s0 = ROR32(w[i - 15], 7) ^ ROR32(w[i - 15], 18) ^ (w[i - 15] >> 3);
        u32 s1 = ROR32(w[i - 2], 17) ^ ROR32(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    u32 a_ = st[0], b_ = st[1], c = st[2], d = st[3], e = st[4], f = st[5], g = st[6], h = st[7];
    for (int i = 0; i < 64; i++) {
        u32 t1 = h + (ROR32(e, 6) ^ ROR32(e, 11) ^ ROR32(e, 25)) + ((e & f) ^ (~e & g)) + K256[i] + w[i];
        u32 t2 = (ROR32(a_, 2) ^ ROR32(a_, 13) ^ ROR32(a_, 22)) + ((a_ & b_) ^ (a_ & c) ^ (b_ & c));
        h = g; g = f; f = e; e = d + t1; d = c; c = b_; b_ = a_; a_ = t1 + t2;
    }
    st[0] += a_; st[1] += b_; st[2] += c; st[3] += d; st[4] += e; st[5] += f; st[6] += g; st[7] += h;
}

static const u64 RC[24] = {
    0x0000000000000001ULL, 0x0000000000008082ULL, 0x800000000000808aULL, 0x8000000080008000ULL,
    0x000000000000808bULL, 0x0000000080000001ULL, 0x8000000080008081ULL, 0x8000000000008009ULL,
    0x000000000000008aULL, 0x0000000000000088ULL, 0x0000000080008009ULL, 0x000000008000000aULL,
    0x000000008000808bULL, 0x800000000000008bULL, 0x8000000000008089ULL, 0x8000000000008003ULL,
    0x8000000000008002ULL, 0x8000000000000080ULL, 0x000000000000800aULL, 0x800000008000000aULL,
    0x8000000080008081ULL, 0x8000000000008080ULL, 0x0000000080000001ULL, 0x8000000080008008ULL,
};
static const int ROTC[24] = { 1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44 };
static const int PILN[24] = { 10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1 };
#define ROL64(x, n) (((x) << (n)) | ((x) >> (64 - (n))))

static void keccakf(u64 st[25])
{
    u64 bc[5];
    for (int round = 0; round < 24; round++) {
        for (int i = 0; i < 5; i++)
            bc[i] = st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20];
        for (int i = 0; i < 5; i++) {
            u64 t = bc[(i + 4) % 5] ^ ROL64(bc[(i + 1) % 5], 1);
            for (int j = 0; j < 25; j += 5)
                st[j + i] ^= t;
        }
        u64 t = st[1];
        for (int i = 0; i < 24; i++) {
            int j = PILN[i];
            u64 tmp = st[j];
            st[j] = ROL64(t, ROTC[i]);
            t = tmp;
        }
        for (int j = 0; j < 25; j += 5) {
            for (int i = 0; i < 5; i++)
                bc[i] = st[j + i];
            for (int i = 0; i < 5; i++)
                st[j + i] ^= (~bc[(i + 1) % 5]) & bc[(i + 2) % 5];
        }
        st[0] ^= RC[round];
    }
}

/* Freestanding: the compiler may emit memset for zero initialization. */
void *memset(void *d, int c, unsigned long n)
{
    unsigned char *p = d;
    while (n--)
        *p++ = (unsigned char)c;
    return d;
}

volatile u64 result;

void bench_main(void)
{
    u64 seed = 0x0123456789abcdefULL;
    for (int i = 0; i < LIMBS; i++) {
        seed = seed * 6364136223846793005ULL + 1442695040888963407ULL;
        a[i] = seed;
        seed = seed * 6364136223846793005ULL + 1442695040888963407ULL;
        b[i] = seed;
    }
    u64 acc = 0;
    /* RSA-like bignum multiplies. */
    for (int it = 0; it < 6000 / SCALE; it++) {
        mul_n(r, a, b, LIMBS);
        a[it % LIMBS] ^= r[LIMBS + (it % LIMBS)];
        acc += r[it % (2 * LIMBS)];
    }
    /* SHA-256 over a 64-byte block. */
    u32 st[8] = { 0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19 };
    unsigned char blk[64];
    for (int i = 0; i < 64; i++)
        blk[i] = (unsigned char)(acc >> (i % 8));
    for (int it = 0; it < 60000 / SCALE; it++) {
        sha256_block(st, blk);
        blk[it % 64] ^= (unsigned char)st[it % 8];
    }
    acc += st[0];
    /* SHA-3 permutations. */
    u64 ks[25] = { 0 };
    ks[0] = acc;
    for (int it = 0; it < 40000 / SCALE; it++)
        keccakf(ks);
    acc += ks[0];
    result = acc;
}
