/* Read-only USB diagnostics; use the selected instance, not just VID/PID.
 * C and Win32 APIs keep this helper free of additional C++ runtime DLLs. */
#include <windows.h>
#include <setupapi.h>
#include <cfgmgr32.h>
#include <initguid.h>
#include <usbiodef.h>
#include <usbioctl.h>
#include <wchar.h>

#define BUFFER_SIZE 65536
#define KEY_COUNT 8
#define KEY_SIZE 1024

static int has_interface_suffix(const WCHAR* id) {
    for (; *id; ++id) if (_wcsnicmp(id, L"&mi_", 4) == 0) return 1;
    return 0;
}

static int ancestor_driver_keys(const WCHAR* instance, WCHAR keys[KEY_COUNT][KEY_SIZE]) {
    DEVINST node = 0;
    int count = 0;
    if (CM_Locate_DevNodeW(&node, (WCHAR*)instance, CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS) return 0;
    for (int depth = 0; depth < KEY_COUNT; ++depth) {
        WCHAR id[MAX_DEVICE_ID_LEN] = {0};
        if (CM_Get_Device_IDW(node, id, MAX_DEVICE_ID_LEN, 0) != CR_SUCCESS || _wcsnicmp(id, L"USB\\", 4) != 0) break;
        ULONG bytes = KEY_SIZE * sizeof(WCHAR), type = 0;
        if (CM_Get_DevNode_Registry_PropertyW(node, CM_DRP_DRIVER, &type, keys[count], &bytes, 0) == CR_SUCCESS
            && type == REG_SZ && keys[count][0]) {
            keys[count][KEY_SIZE - 1] = 0;
            ++count;
        }
        /* Stop at the physical device; an ancestor hub's link is not this card's link. */
        if (_wcsnicmp(id, L"USB\\VID_", 8) == 0 && !has_interface_suffix(id)) break;
        DEVINST parent = 0;
        if (CM_Get_Parent(&parent, node, 0) != CR_SUCCESS) break;
        node = parent;
    }
    return count;
}

static int speed_on_hub(HANDLE hub, WCHAR keys[KEY_COUNT][KEY_SIZE], int count, BYTE* buffer) {
    USB_NODE_INFORMATION node = {0};
    DWORD returned = 0;
    node.NodeType = UsbHub;
    if (!DeviceIoControl(hub, IOCTL_USB_GET_NODE_INFORMATION, &node, sizeof(node), &node, sizeof(node), &returned, NULL)) return 0;
    for (ULONG port = 1; port <= node.u.HubInformation.HubDescriptor.bNumberOfPorts; ++port) {
        ZeroMemory(buffer, BUFFER_SIZE);
        USB_NODE_CONNECTION_DRIVERKEY_NAME* driver = (USB_NODE_CONNECTION_DRIVERKEY_NAME*)buffer;
        driver->ConnectionIndex = port;
        if (!DeviceIoControl(hub, IOCTL_USB_GET_NODE_CONNECTION_DRIVERKEY_NAME, driver, sizeof(*driver),
            driver, BUFFER_SIZE, &returned, NULL)) continue;
        if (driver->ActualLength < sizeof(*driver) || driver->ActualLength > returned || returned > BUFFER_SIZE) continue;
        ((WCHAR*)buffer)[BUFFER_SIZE / sizeof(WCHAR) - 1] = 0;
        int matched = 0;
        for (int key = 0; key < count; ++key) if (_wcsicmp(driver->DriverKeyName, keys[key]) == 0) { matched = 1; break; }
        if (!matched) continue;
        ZeroMemory(buffer, BUFFER_SIZE);
        USB_NODE_CONNECTION_INFORMATION_EX* info = (USB_NODE_CONNECTION_INFORMATION_EX*)buffer;
        info->ConnectionIndex = port;
        if (!DeviceIoControl(hub, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX,
            info, BUFFER_SIZE, info, BUFFER_SIZE, &returned, NULL) || info->ConnectionStatus != DeviceConnected) return 0;
        UCHAR speed = info->Speed;
        USB_NODE_CONNECTION_INFORMATION_EX_V2 v2 = {0};
        v2.ConnectionIndex = port;
        v2.Length = sizeof(v2);
        v2.SupportedUsbProtocols.Usb300 = 1;
        /* EX alone also reports High-Speed on USB 3. V2 confirmation is mandatory. */
        if (!DeviceIoControl(hub, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2,
            &v2, sizeof(v2), &v2, sizeof(v2), &returned, NULL)) return 0;
        if (v2.Flags.DeviceIsOperatingAtSuperSpeedPlusOrHigher) return 5;
        if (v2.Flags.DeviceIsOperatingAtSuperSpeedOrHigher) return 4;
        return speed == UsbHighSpeed ? 3 : (speed == UsbFullSpeed ? 2 : (speed == UsbLowSpeed ? 1 : 0));
    }
    return 0;
}

int tacklecast_usb_link(const WCHAR* instance) {
    WCHAR keys[KEY_COUNT][KEY_SIZE] = {{0}};
    int count = ancestor_driver_keys(instance, keys);
    if (!count) return 0;
    HDEVINFO devices = SetupDiGetClassDevsW(&GUID_DEVINTERFACE_USB_HUB, NULL, NULL, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
    if (devices == INVALID_HANDLE_VALUE) return 0;
    BYTE* buffer = (BYTE*)HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, BUFFER_SIZE);
    if (!buffer) { SetupDiDestroyDeviceInfoList(devices); return 0; }
    int speed = 0;
    for (DWORD index = 0; index < 1024; ++index) {
        SP_DEVICE_INTERFACE_DATA iface = {0};
        iface.cbSize = sizeof(iface);
        if (!SetupDiEnumDeviceInterfaces(devices, NULL, &GUID_DEVINTERFACE_USB_HUB, index, &iface)) break;
        DWORD needed = 0;
        SetupDiGetDeviceInterfaceDetailW(devices, &iface, NULL, 0, &needed, NULL);
        if (needed < sizeof(SP_DEVICE_INTERFACE_DETAIL_DATA_W) || needed > BUFFER_SIZE) continue;
        SP_DEVICE_INTERFACE_DETAIL_DATA_W* detail = (SP_DEVICE_INTERFACE_DETAIL_DATA_W*)HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, needed);
        if (!detail) break;
        detail->cbSize = sizeof(*detail);
        HANDLE hub = INVALID_HANDLE_VALUE;
        if (SetupDiGetDeviceInterfaceDetailW(devices, &iface, detail, needed, NULL, NULL)) {
            hub = CreateFileW(detail->DevicePath, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING, 0, NULL);
        }
        HeapFree(GetProcessHeap(), 0, detail);
        if (hub == INVALID_HANDLE_VALUE) continue;
        speed = speed_on_hub(hub, keys, count, buffer);
        CloseHandle(hub);
        if (speed) break;
    }
    HeapFree(GetProcessHeap(), 0, buffer);
    SetupDiDestroyDeviceInfoList(devices);
    return speed;
}
