using System;
using System.IO;
using System.Text;
using UnityEngine;

namespace MistLib
{
    public static class BincodeWriter
    {
        public static byte[] SerializeLocation(string objId, Vector3 pos, Vector3 rot, Vector3 vel, float time, ushort seq)
        {
            using var ms = new MemoryStream();
            using var writer = new BinaryWriter(ms);
            
            var idBytes = Encoding.UTF8.GetBytes(objId);
            writer.Write((ulong)idBytes.Length);
            writer.Write(idBytes);

            writer.Write(pos.x);
            writer.Write(pos.y);
            writer.Write(pos.z);

            writer.Write(rot.x);
            writer.Write(rot.y);
            writer.Write(rot.z);

            writer.Write(vel.x);
            writer.Write(vel.y);
            writer.Write(vel.z);

            writer.Write(time);
            writer.Write(seq);
            
            return ms.ToArray();
        }
    }

    public static class BincodeReader
    {
        public static void DeserializeLocation(byte[] data, out string objId, out Vector3 pos, out Vector3 rot, out Vector3 vel, out float time, out ushort seq)
        {
            using var ms = new MemoryStream(data);
            using var reader = new BinaryReader(ms);
            
            var len = reader.ReadUInt64();
            var idBytes = reader.ReadBytes((int)len);
            objId = Encoding.UTF8.GetString(idBytes);

            pos = new Vector3(reader.ReadSingle(), reader.ReadSingle(), reader.ReadSingle());
            rot = new Vector3(reader.ReadSingle(), reader.ReadSingle(), reader.ReadSingle());
            vel = new Vector3(reader.ReadSingle(), reader.ReadSingle(), reader.ReadSingle());
            time = reader.ReadSingle();
            seq = reader.ReadUInt16();
        }
    }
}
