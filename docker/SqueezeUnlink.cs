using System;
using System.Text;
using System.Buffers;
using System.Collections.Generic;
using Garnet.common;
using Garnet.server;
using Tsavorite.core;

namespace SqueezeExtensions
{
    public class SqueezeUnlink : CustomTransactionProcedure
    {
        public override bool Prepare<TGarnetReadApi>(TGarnetReadApi api, ref CustomProcedureInput procInput)
        {
            var parseState = procInput.parseState;
            if (parseState.Count < 6)
                return false;

            var metaKey = parseState.Parameters[0];
            var blockMapKey = parseState.Parameters[1];
            var refcountsKey = parseState.Parameters[2];
            var sizesKey = parseState.Parameters[3];
            var freeBlocksKey = parseState.Parameters[4];
            var typeField = parseState.Parameters[5];

            AddKey(metaKey, LockType.Exclusive, StoreType.Object);
            AddKey(blockMapKey, LockType.Exclusive, StoreType.Object);
            AddKey(refcountsKey, LockType.Exclusive, StoreType.Object);
            AddKey(sizesKey, LockType.Exclusive, StoreType.Object);
            AddKey(freeBlocksKey, LockType.Exclusive, StoreType.Object);

            return true;
        }

        public override void Main<TGarnetApi>(TGarnetApi api, ref CustomProcedureInput procInput, ref MemoryResult<byte> output)
        {
            var parseState = procInput.parseState;
            var metaKey = parseState.Parameters[0];
            var blockMapKey = parseState.Parameters[1];
            var refcountsKey = parseState.Parameters[2];
            var sizesKey = parseState.Parameters[3];
            var freeBlocksKey = parseState.Parameters[4];
            var typeField = parseState.Parameters[5];

            if (api.HashGet(metaKey, typeField, out var fileTypeSlice) == GarnetStatus.OK)
            {
                string fileType = Encoding.UTF8.GetString(fileTypeSlice.Span);

                List<string> deletedBlocks = new List<string>();

                if (fileType == "striped")
                {
                    if (api.HashGetAll(blockMapKey, out var mappings) == GarnetStatus.OK)
                    {
                        if (mappings != null && mappings.Length > 0)
                        {
                            bool isValue = false;
                            foreach (var item in mappings)
                            {
                                if (isValue)
                                {
                                    var blockKeySlice = item;
                                    string blockKeyStr = Encoding.UTF8.GetString(blockKeySlice.Span);

                                    int refcount = 1;
                                    if (api.HashGet(refcountsKey, blockKeySlice, out var refcountSlice) == GarnetStatus.OK)
                                    {
                                        int.TryParse(Encoding.UTF8.GetString(refcountSlice.Span), out refcount);
                                    }

                                    refcount--;
                                    if (refcount <= 0)
                                    {
                                        api.HashDelete(refcountsKey, blockKeySlice, out _);
                                        api.HashDelete(sizesKey, blockKeySlice, out _);
                                        api.SetAdd(freeBlocksKey, blockKeySlice, out _);
                                        deletedBlocks.Add(blockKeyStr);
                                    }
                                    else
                                    {
                                        var refcountBytes = Encoding.UTF8.GetBytes(refcount.ToString());
                                        var refcountVal = CreateArgSlice(refcountBytes);
                                        api.HashSet(refcountsKey, blockKeySlice, refcountVal, out _);
                                    }
                                }
                                isValue = !isValue;
                            }
                        }
                    }
                    api.DELETE(blockMapKey);
                }
                api.DELETE(metaKey);

                // Format RESP array manually
                var sb = new StringBuilder();
                sb.Append($"*{deletedBlocks.Count}\r\n");
                foreach (var b in deletedBlocks)
                {
                    sb.Append($"${b.Length}\r\n{b}\r\n");
                }
                byte[] respBytes = Encoding.UTF8.GetBytes(sb.ToString());

                output.MemoryOwner?.Dispose();
                output.Length = respBytes.Length;
                output.MemoryOwner = MemoryPool<byte>.Shared.Rent(respBytes.Length);
                respBytes.CopyTo(output.MemoryOwner.Memory.Span);
            }
        }
    }
}
