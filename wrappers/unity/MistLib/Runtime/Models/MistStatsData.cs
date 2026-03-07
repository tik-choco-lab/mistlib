using System;
using System.Collections.Generic;

namespace MistLib
{
    [Serializable]
    public class MistStatsData
    {
        public int messageCount;
        public int sendBits;
        public int receiveBits;
        public Dictionary<string, float> rttMillis = new();
        public int evalSendBits;
        public int evalReceiveBits;
        public int evalMessageCount;
    }
}
