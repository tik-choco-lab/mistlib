namespace MistLib
{
    public enum MistLibMessageType : uint
    {
        Location = 0,
        Message = 1,
        Neighbors = 2,
        
        Heartbeat = 100,
        RequestNodeList = 101,
        NodeList = 102
    }
}
