# Video frame fragmentation: encoder slice mode (NAL-unit-per-datagram)

QUIC datagrams are bounded by path MTU (~1200 bytes), but H.264 I-frames can reach 100–200 KB at 1080p 15 Mbps CBR. We need a strategy for frames that exceed MTU.

We decided to use encoder-level slice mode as the primary strategy: tell FFmpeg to encode each frame as multiple independently-decodable slices, each fitting within one datagram. The H.264 decoder on the receiving end reassembles slices natively.

A protocol-level fragmentation trait (`FrameFragmenter`) is defined as a fallback for hardware encoders that don't support slice mode (notably AMD AMF). The default `NoOpFragmenter` assumes all NAL units fit within MTU; a real fragmenter can be swapped in later via the trait without changing callers.

**Considered alternative:** Full protocol-level fragmentation (sequence numbers, reassembly buffer, timeout handling). Rejected because it adds significant complexity to the transport layer for a case that only triggers on specific hardware. Slice mode covers the common path (NVENC, QSV) with zero extra code.
