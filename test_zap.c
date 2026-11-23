#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* 单块 FFI 接口：压缩 → 解压 → 逐字节比对。
   order / mem_mb 必须两边一致。 */
extern int zap_compress_buf(const unsigned char* data, unsigned long len,
                            unsigned int order, unsigned int mem_mb,
                            unsigned char** out_ptr, unsigned long* out_len);
extern int zap_decompress_buf(const unsigned char* data, unsigned long len,
                              unsigned long unpacked_len, unsigned int order,
                              unsigned int mem_mb, unsigned char** out_ptr,
                              unsigned long* out_len);
extern void zap_free(unsigned char* ptr, unsigned long len);

int main(void) {
    const char* msg = "hello zap ffi hello zap ffi hello zap ffi hello zap ffi";
    unsigned long n = strlen(msg);
    unsigned char* packed = NULL;
    unsigned long plen = 0;
    int rc = zap_compress_buf((const unsigned char*)msg, n, 6, 16, &packed, &plen);
    printf("compress rc=%d in=%lu out=%lu\n", rc, n, plen);
    if (rc != 0) return rc;

    unsigned char* out = NULL;
    unsigned long olen = 0;
    rc = zap_decompress_buf(packed, plen, n, 6, 16, &out, &olen);
    printf("decompress rc=%d out=%lu equal=%d\n", rc, olen,
           (olen == n && memcmp(out, msg, n) == 0));

    zap_free(packed, plen);
    zap_free(out, olen);
    return rc;
}
