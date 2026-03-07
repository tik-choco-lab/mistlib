using System;
using System.Runtime.InteropServices;

namespace MistLib
{
    public static class MistFFI
    {
        private const string DllName = "mistlib";

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        public delegate void EventCallback(uint eventType, IntPtr from, UIntPtr fromLen, IntPtr data, UIntPtr dataLen);

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        public delegate void LogCallback(uint level, IntPtr data, UIntPtr len);

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern void register_event_callback(EventCallback cb);

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern void register_log_callback(LogCallback cb);

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern void init(IntPtr idPtr, UIntPtr idLen, IntPtr urlPtr, UIntPtr urlLen);

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern void join_room(IntPtr roomPtr, UIntPtr roomLen);

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern void leave_room();

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern void update_position(float x, float y, float z);

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern void on_connected(IntPtr nodePtr, UIntPtr nodeLen);

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern void on_disconnected(IntPtr nodePtr, UIntPtr nodeLen);

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern void set_config(IntPtr data, UIntPtr len);

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern void send_message(IntPtr targetPtr, UIntPtr targetLen, IntPtr dataPtr, UIntPtr dataLen, uint method);

        [DllImport(DllName, CallingConvention = CallingConvention.Cdecl)]
        public static extern uint get_stats(IntPtr buffer, UIntPtr bufferLen);
    }
}
