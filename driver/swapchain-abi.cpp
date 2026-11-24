#define WIN32_NO_STATUS
#include <Windows.h>
#undef WIN32_NO_STATUS
#include <ntstatus.h>
#define WDF_STUB
#include <wdf.h>
#define IDD_STUB
#include <IddCx.h>
#include <cstddef>
#include <cstdio>

#define LAYOUT(Type) \
    std::printf(#Type ".size=%zu\n", sizeof(Type)); \
    std::printf(#Type ".align=%zu\n", alignof(Type))
#define FIELD(Type, Member) \
    std::printf(#Type "." #Member "=%zu\n", offsetof(Type, Member))

int main() {
    LAYOUT(IDARG_IN_SWAPCHAINSETDEVICE);
    FIELD(IDARG_IN_SWAPCHAINSETDEVICE, pDevice);
    LAYOUT(IDARG_OUT_RELEASEANDACQUIREBUFFER);
    FIELD(IDARG_OUT_RELEASEANDACQUIREBUFFER, MetaData);
    LAYOUT(IDDCX_METADATA);
    FIELD(IDDCX_METADATA, Size);
    FIELD(IDDCX_METADATA, PresentationFrameNumber);
    FIELD(IDDCX_METADATA, DirtyRectCount);
    FIELD(IDDCX_METADATA, MoveRegionCount);
    FIELD(IDDCX_METADATA, HwProtectedSurface);
    FIELD(IDDCX_METADATA, pSurface);
}
