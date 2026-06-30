using System.Text;
using Garnet.common;
using Garnet.server;

namespace SqueezeExtensions
{
    public class SqueezeUnlink : CustomTransactionProcedure
    {
        public override bool Prepare<TGarnetLoadApi>(TGarnetLoadApi api, ArgSlice input)
        {
            // In a real implementation, keys involved are registered for server-side locking.
            return true;
        }

        public override void Main<TGarnetApi>(TGarnetApi api, ArgSlice input, ref MemoryResult<byte> output)
        {
            // Perform atomic unlink transaction steps directly on the server.
        }
    }
}
