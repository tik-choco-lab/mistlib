using UnityEngine;

namespace MistLib
{
    public class MistStats : MonoBehaviour
    {
        public static MistStats I { get; private set; }
        
        public MistStatsData StatData { get; private set; }

        private void Awake()
        {
            if (I != null)
            {
                Destroy(gameObject);
                return;
            }
            I = this;
            DontDestroyOnLoad(gameObject);
            
            StatData = new MistStatsData();
        }

        private void Update()
        {
            if (Time.frameCount % 60 == 0) // Update roughly every second
            {
                UpdateStats();
            }
        }

        public void UpdateStats()
        {
            if (MistEngine.I == null) return;
            
            var json = MistEngine.I.GetStats();
            if (!string.IsNullOrEmpty(json) && json != "{}")
            {
                JsonUtility.FromJsonOverwrite(json, StatData);
            }
        }
    }
}
