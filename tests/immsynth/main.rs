// Generated constant-materialization correctness test (PLIRON_IMMSYNTH).
// Every function materializes its constant through the backend's
// synthesis and prints it; test.sh diffs the output against the
// golden file, which is independent of the compiler.
use std::hint::black_box;

#[inline(never)]
fn k0() -> u64 { 0x5c5c5c5c5c5c5c5cu64 }
#[inline(never)]
fn k1() -> u64 { 0xfefefefefefefeffu64 }
#[inline(never)]
fn k2() -> u64 { 0xdfdfdfdfdfdfdfe0u64 }
#[inline(never)]
fn k3() -> u64 { 0x0000000000000000u64 }
#[inline(never)]
fn k4() -> u64 { 0x0101010101010101u64 }
#[inline(never)]
fn k5() -> u64 { 0x0202020202020202u64 }
#[inline(never)]
fn k6() -> u64 { 0x0303030303030303u64 }
#[inline(never)]
fn k7() -> u64 { 0x0404040404040404u64 }
#[inline(never)]
fn k8() -> u64 { 0x0505050505050505u64 }
#[inline(never)]
fn k9() -> u64 { 0x0606060606060606u64 }
#[inline(never)]
fn k10() -> u64 { 0x0707070707070707u64 }
#[inline(never)]
fn k11() -> u64 { 0x0808080808080808u64 }
#[inline(never)]
fn k12() -> u64 { 0x0909090909090909u64 }
#[inline(never)]
fn k13() -> u64 { 0x0a0a0a0a0a0a0a0au64 }
#[inline(never)]
fn k14() -> u64 { 0x0b0b0b0b0b0b0b0bu64 }
#[inline(never)]
fn k15() -> u64 { 0x0c0c0c0c0c0c0c0cu64 }
#[inline(never)]
fn k16() -> u64 { 0x0d0d0d0d0d0d0d0du64 }
#[inline(never)]
fn k17() -> u64 { 0x0e0e0e0e0e0e0e0eu64 }
#[inline(never)]
fn k18() -> u64 { 0x0f0f0f0f0f0f0f0fu64 }
#[inline(never)]
fn k19() -> u64 { 0x1010101010101010u64 }
#[inline(never)]
fn k20() -> u64 { 0x1111111111111111u64 }
#[inline(never)]
fn k21() -> u64 { 0x1212121212121212u64 }
#[inline(never)]
fn k22() -> u64 { 0x1313131313131313u64 }
#[inline(never)]
fn k23() -> u64 { 0x1414141414141414u64 }
#[inline(never)]
fn k24() -> u64 { 0x1515151515151515u64 }
#[inline(never)]
fn k25() -> u64 { 0x1616161616161616u64 }
#[inline(never)]
fn k26() -> u64 { 0x1717171717171717u64 }
#[inline(never)]
fn k27() -> u64 { 0x1818181818181818u64 }
#[inline(never)]
fn k28() -> u64 { 0x1919191919191919u64 }
#[inline(never)]
fn k29() -> u64 { 0x1a1a1a1a1a1a1a1au64 }
#[inline(never)]
fn k30() -> u64 { 0x1b1b1b1b1b1b1b1bu64 }
#[inline(never)]
fn k31() -> u64 { 0x1c1c1c1c1c1c1c1cu64 }
#[inline(never)]
fn k32() -> u64 { 0x1d1d1d1d1d1d1d1du64 }
#[inline(never)]
fn k33() -> u64 { 0x1e1e1e1e1e1e1e1eu64 }
#[inline(never)]
fn k34() -> u64 { 0x1f1f1f1f1f1f1f1fu64 }
#[inline(never)]
fn k35() -> u64 { 0x2020202020202020u64 }
#[inline(never)]
fn k36() -> u64 { 0x2121212121212121u64 }
#[inline(never)]
fn k37() -> u64 { 0x2222222222222222u64 }
#[inline(never)]
fn k38() -> u64 { 0x2323232323232323u64 }
#[inline(never)]
fn k39() -> u64 { 0x2424242424242424u64 }
#[inline(never)]
fn k40() -> u64 { 0x2525252525252525u64 }
#[inline(never)]
fn k41() -> u64 { 0x2626262626262626u64 }
#[inline(never)]
fn k42() -> u64 { 0x2727272727272727u64 }
#[inline(never)]
fn k43() -> u64 { 0x2828282828282828u64 }
#[inline(never)]
fn k44() -> u64 { 0x2929292929292929u64 }
#[inline(never)]
fn k45() -> u64 { 0x2a2a2a2a2a2a2a2au64 }
#[inline(never)]
fn k46() -> u64 { 0x2b2b2b2b2b2b2b2bu64 }
#[inline(never)]
fn k47() -> u64 { 0x2c2c2c2c2c2c2c2cu64 }
#[inline(never)]
fn k48() -> u64 { 0x2d2d2d2d2d2d2d2du64 }
#[inline(never)]
fn k49() -> u64 { 0x2e2e2e2e2e2e2e2eu64 }
#[inline(never)]
fn k50() -> u64 { 0x2f2f2f2f2f2f2f2fu64 }
#[inline(never)]
fn k51() -> u64 { 0x3030303030303030u64 }
#[inline(never)]
fn k52() -> u64 { 0x3131313131313131u64 }
#[inline(never)]
fn k53() -> u64 { 0x3232323232323232u64 }
#[inline(never)]
fn k54() -> u64 { 0x3333333333333333u64 }
#[inline(never)]
fn k55() -> u64 { 0x3434343434343434u64 }
#[inline(never)]
fn k56() -> u64 { 0x3535353535353535u64 }
#[inline(never)]
fn k57() -> u64 { 0x3636363636363636u64 }
#[inline(never)]
fn k58() -> u64 { 0x3737373737373737u64 }
#[inline(never)]
fn k59() -> u64 { 0x3838383838383838u64 }
#[inline(never)]
fn k60() -> u64 { 0x3939393939393939u64 }
#[inline(never)]
fn k61() -> u64 { 0x3a3a3a3a3a3a3a3au64 }
#[inline(never)]
fn k62() -> u64 { 0x3b3b3b3b3b3b3b3bu64 }
#[inline(never)]
fn k63() -> u64 { 0x3c3c3c3c3c3c3c3cu64 }
#[inline(never)]
fn k64() -> u64 { 0x3d3d3d3d3d3d3d3du64 }
#[inline(never)]
fn k65() -> u64 { 0x3e3e3e3e3e3e3e3eu64 }
#[inline(never)]
fn k66() -> u64 { 0x3f3f3f3f3f3f3f3fu64 }
#[inline(never)]
fn k67() -> u64 { 0x4040404040404040u64 }
#[inline(never)]
fn k68() -> u64 { 0x4141414141414141u64 }
#[inline(never)]
fn k69() -> u64 { 0x4242424242424242u64 }
#[inline(never)]
fn k70() -> u64 { 0x4343434343434343u64 }
#[inline(never)]
fn k71() -> u64 { 0x4444444444444444u64 }
#[inline(never)]
fn k72() -> u64 { 0x4545454545454545u64 }
#[inline(never)]
fn k73() -> u64 { 0x4646464646464646u64 }
#[inline(never)]
fn k74() -> u64 { 0x4747474747474747u64 }
#[inline(never)]
fn k75() -> u64 { 0x4848484848484848u64 }
#[inline(never)]
fn k76() -> u64 { 0x4949494949494949u64 }
#[inline(never)]
fn k77() -> u64 { 0x4a4a4a4a4a4a4a4au64 }
#[inline(never)]
fn k78() -> u64 { 0x4b4b4b4b4b4b4b4bu64 }
#[inline(never)]
fn k79() -> u64 { 0x4c4c4c4c4c4c4c4cu64 }
#[inline(never)]
fn k80() -> u64 { 0x4d4d4d4d4d4d4d4du64 }
#[inline(never)]
fn k81() -> u64 { 0x4e4e4e4e4e4e4e4eu64 }
#[inline(never)]
fn k82() -> u64 { 0x4f4f4f4f4f4f4f4fu64 }
#[inline(never)]
fn k83() -> u64 { 0x5050505050505050u64 }
#[inline(never)]
fn k84() -> u64 { 0x5151515151515151u64 }
#[inline(never)]
fn k85() -> u64 { 0x5252525252525252u64 }
#[inline(never)]
fn k86() -> u64 { 0x5353535353535353u64 }
#[inline(never)]
fn k87() -> u64 { 0x5454545454545454u64 }
#[inline(never)]
fn k88() -> u64 { 0x5555555555555555u64 }
#[inline(never)]
fn k89() -> u64 { 0x5656565656565656u64 }
#[inline(never)]
fn k90() -> u64 { 0x5757575757575757u64 }
#[inline(never)]
fn k91() -> u64 { 0x5858585858585858u64 }
#[inline(never)]
fn k92() -> u64 { 0x5959595959595959u64 }
#[inline(never)]
fn k93() -> u64 { 0x5a5a5a5a5a5a5a5au64 }
#[inline(never)]
fn k94() -> u64 { 0x5b5b5b5b5b5b5b5bu64 }
#[inline(never)]
fn k95() -> u64 { 0x5d5d5d5d5d5d5d5du64 }
#[inline(never)]
fn k96() -> u64 { 0x5e5e5e5e5e5e5e5eu64 }
#[inline(never)]
fn k97() -> u64 { 0x5f5f5f5f5f5f5f5fu64 }
#[inline(never)]
fn k98() -> u64 { 0x6060606060606060u64 }
#[inline(never)]
fn k99() -> u64 { 0x6161616161616161u64 }
#[inline(never)]
fn k100() -> u64 { 0x6262626262626262u64 }
#[inline(never)]
fn k101() -> u64 { 0x6363636363636363u64 }
#[inline(never)]
fn k102() -> u64 { 0x6464646464646464u64 }
#[inline(never)]
fn k103() -> u64 { 0x6565656565656565u64 }
#[inline(never)]
fn k104() -> u64 { 0x6666666666666666u64 }
#[inline(never)]
fn k105() -> u64 { 0x6767676767676767u64 }
#[inline(never)]
fn k106() -> u64 { 0x6868686868686868u64 }
#[inline(never)]
fn k107() -> u64 { 0x6969696969696969u64 }
#[inline(never)]
fn k108() -> u64 { 0x6a6a6a6a6a6a6a6au64 }
#[inline(never)]
fn k109() -> u64 { 0x6b6b6b6b6b6b6b6bu64 }
#[inline(never)]
fn k110() -> u64 { 0x6c6c6c6c6c6c6c6cu64 }
#[inline(never)]
fn k111() -> u64 { 0x6d6d6d6d6d6d6d6du64 }
#[inline(never)]
fn k112() -> u64 { 0x6e6e6e6e6e6e6e6eu64 }
#[inline(never)]
fn k113() -> u64 { 0x6f6f6f6f6f6f6f6fu64 }
#[inline(never)]
fn k114() -> u64 { 0x7070707070707070u64 }
#[inline(never)]
fn k115() -> u64 { 0x7171717171717171u64 }
#[inline(never)]
fn k116() -> u64 { 0x7272727272727272u64 }
#[inline(never)]
fn k117() -> u64 { 0x7373737373737373u64 }
#[inline(never)]
fn k118() -> u64 { 0x7474747474747474u64 }
#[inline(never)]
fn k119() -> u64 { 0x7575757575757575u64 }
#[inline(never)]
fn k120() -> u64 { 0x7676767676767676u64 }
#[inline(never)]
fn k121() -> u64 { 0x7777777777777777u64 }
#[inline(never)]
fn k122() -> u64 { 0x7878787878787878u64 }
#[inline(never)]
fn k123() -> u64 { 0x7979797979797979u64 }
#[inline(never)]
fn k124() -> u64 { 0x7a7a7a7a7a7a7a7au64 }
#[inline(never)]
fn k125() -> u64 { 0x7b7b7b7b7b7b7b7bu64 }
#[inline(never)]
fn k126() -> u64 { 0x7c7c7c7c7c7c7c7cu64 }
#[inline(never)]
fn k127() -> u64 { 0x7d7d7d7d7d7d7d7du64 }
#[inline(never)]
fn k128() -> u64 { 0x7e7e7e7e7e7e7e7eu64 }
#[inline(never)]
fn k129() -> u64 { 0x7f7f7f7f7f7f7f7fu64 }
#[inline(never)]
fn k130() -> u64 { 0x8080808080808080u64 }
#[inline(never)]
fn k131() -> u64 { 0x8181818181818181u64 }
#[inline(never)]
fn k132() -> u64 { 0x8282828282828282u64 }
#[inline(never)]
fn k133() -> u64 { 0x8383838383838383u64 }
#[inline(never)]
fn k134() -> u64 { 0x8484848484848484u64 }
#[inline(never)]
fn k135() -> u64 { 0x8585858585858585u64 }
#[inline(never)]
fn k136() -> u64 { 0x8686868686868686u64 }
#[inline(never)]
fn k137() -> u64 { 0x8787878787878787u64 }
#[inline(never)]
fn k138() -> u64 { 0x8888888888888888u64 }
#[inline(never)]
fn k139() -> u64 { 0x8989898989898989u64 }
#[inline(never)]
fn k140() -> u64 { 0x8a8a8a8a8a8a8a8au64 }
#[inline(never)]
fn k141() -> u64 { 0x8b8b8b8b8b8b8b8bu64 }
#[inline(never)]
fn k142() -> u64 { 0x8c8c8c8c8c8c8c8cu64 }
#[inline(never)]
fn k143() -> u64 { 0x8d8d8d8d8d8d8d8du64 }
#[inline(never)]
fn k144() -> u64 { 0x8e8e8e8e8e8e8e8eu64 }
#[inline(never)]
fn k145() -> u64 { 0x8f8f8f8f8f8f8f8fu64 }
#[inline(never)]
fn k146() -> u64 { 0x9090909090909090u64 }
#[inline(never)]
fn k147() -> u64 { 0x9191919191919191u64 }
#[inline(never)]
fn k148() -> u64 { 0x9292929292929292u64 }
#[inline(never)]
fn k149() -> u64 { 0x9393939393939393u64 }
#[inline(never)]
fn k150() -> u64 { 0x9494949494949494u64 }
#[inline(never)]
fn k151() -> u64 { 0x9595959595959595u64 }
#[inline(never)]
fn k152() -> u64 { 0x9696969696969696u64 }
#[inline(never)]
fn k153() -> u64 { 0x9797979797979797u64 }
#[inline(never)]
fn k154() -> u64 { 0x9898989898989898u64 }
#[inline(never)]
fn k155() -> u64 { 0x9999999999999999u64 }
#[inline(never)]
fn k156() -> u64 { 0x9a9a9a9a9a9a9a9au64 }
#[inline(never)]
fn k157() -> u64 { 0x9b9b9b9b9b9b9b9bu64 }
#[inline(never)]
fn k158() -> u64 { 0x9c9c9c9c9c9c9c9cu64 }
#[inline(never)]
fn k159() -> u64 { 0x9d9d9d9d9d9d9d9du64 }
#[inline(never)]
fn k160() -> u64 { 0x9e9e9e9e9e9e9e9eu64 }
#[inline(never)]
fn k161() -> u64 { 0x9f9f9f9f9f9f9f9fu64 }
#[inline(never)]
fn k162() -> u64 { 0xa0a0a0a0a0a0a0a0u64 }
#[inline(never)]
fn k163() -> u64 { 0xa1a1a1a1a1a1a1a1u64 }
#[inline(never)]
fn k164() -> u64 { 0xa2a2a2a2a2a2a2a2u64 }
#[inline(never)]
fn k165() -> u64 { 0xa3a3a3a3a3a3a3a3u64 }
#[inline(never)]
fn k166() -> u64 { 0xa4a4a4a4a4a4a4a4u64 }
#[inline(never)]
fn k167() -> u64 { 0xa5a5a5a5a5a5a5a5u64 }
#[inline(never)]
fn k168() -> u64 { 0xa6a6a6a6a6a6a6a6u64 }
#[inline(never)]
fn k169() -> u64 { 0xa7a7a7a7a7a7a7a7u64 }
#[inline(never)]
fn k170() -> u64 { 0xa8a8a8a8a8a8a8a8u64 }
#[inline(never)]
fn k171() -> u64 { 0xa9a9a9a9a9a9a9a9u64 }
#[inline(never)]
fn k172() -> u64 { 0xaaaaaaaaaaaaaaaau64 }
#[inline(never)]
fn k173() -> u64 { 0xababababababababu64 }
#[inline(never)]
fn k174() -> u64 { 0xacacacacacacacacu64 }
#[inline(never)]
fn k175() -> u64 { 0xadadadadadadadadu64 }
#[inline(never)]
fn k176() -> u64 { 0xaeaeaeaeaeaeaeaeu64 }
#[inline(never)]
fn k177() -> u64 { 0xafafafafafafafafu64 }
#[inline(never)]
fn k178() -> u64 { 0xb0b0b0b0b0b0b0b0u64 }
#[inline(never)]
fn k179() -> u64 { 0xb1b1b1b1b1b1b1b1u64 }
#[inline(never)]
fn k180() -> u64 { 0xb2b2b2b2b2b2b2b2u64 }
#[inline(never)]
fn k181() -> u64 { 0xb3b3b3b3b3b3b3b3u64 }
#[inline(never)]
fn k182() -> u64 { 0xb4b4b4b4b4b4b4b4u64 }
#[inline(never)]
fn k183() -> u64 { 0xb5b5b5b5b5b5b5b5u64 }
#[inline(never)]
fn k184() -> u64 { 0xb6b6b6b6b6b6b6b6u64 }
#[inline(never)]
fn k185() -> u64 { 0xb7b7b7b7b7b7b7b7u64 }
#[inline(never)]
fn k186() -> u64 { 0xb8b8b8b8b8b8b8b8u64 }
#[inline(never)]
fn k187() -> u64 { 0xb9b9b9b9b9b9b9b9u64 }
#[inline(never)]
fn k188() -> u64 { 0xbabababababababau64 }
#[inline(never)]
fn k189() -> u64 { 0xbbbbbbbbbbbbbbbbu64 }
#[inline(never)]
fn k190() -> u64 { 0xbcbcbcbcbcbcbcbcu64 }
#[inline(never)]
fn k191() -> u64 { 0xbdbdbdbdbdbdbdbdu64 }
#[inline(never)]
fn k192() -> u64 { 0xbebebebebebebebeu64 }
#[inline(never)]
fn k193() -> u64 { 0xbfbfbfbfbfbfbfbfu64 }
#[inline(never)]
fn k194() -> u64 { 0xc0c0c0c0c0c0c0c0u64 }
#[inline(never)]
fn k195() -> u64 { 0xc1c1c1c1c1c1c1c1u64 }
#[inline(never)]
fn k196() -> u64 { 0xc2c2c2c2c2c2c2c2u64 }
#[inline(never)]
fn k197() -> u64 { 0xc3c3c3c3c3c3c3c3u64 }
#[inline(never)]
fn k198() -> u64 { 0xc4c4c4c4c4c4c4c4u64 }
#[inline(never)]
fn k199() -> u64 { 0xc5c5c5c5c5c5c5c5u64 }
#[inline(never)]
fn k200() -> u64 { 0xc6c6c6c6c6c6c6c6u64 }
#[inline(never)]
fn k201() -> u64 { 0xc7c7c7c7c7c7c7c7u64 }
#[inline(never)]
fn k202() -> u64 { 0xc8c8c8c8c8c8c8c8u64 }
#[inline(never)]
fn k203() -> u64 { 0xc9c9c9c9c9c9c9c9u64 }
#[inline(never)]
fn k204() -> u64 { 0xcacacacacacacacau64 }
#[inline(never)]
fn k205() -> u64 { 0xcbcbcbcbcbcbcbcbu64 }
#[inline(never)]
fn k206() -> u64 { 0xccccccccccccccccu64 }
#[inline(never)]
fn k207() -> u64 { 0xcdcdcdcdcdcdcdcdu64 }
#[inline(never)]
fn k208() -> u64 { 0xcecececececececeu64 }
#[inline(never)]
fn k209() -> u64 { 0xcfcfcfcfcfcfcfcfu64 }
#[inline(never)]
fn k210() -> u64 { 0xd0d0d0d0d0d0d0d0u64 }
#[inline(never)]
fn k211() -> u64 { 0xd1d1d1d1d1d1d1d1u64 }
#[inline(never)]
fn k212() -> u64 { 0xd2d2d2d2d2d2d2d2u64 }
#[inline(never)]
fn k213() -> u64 { 0xd3d3d3d3d3d3d3d3u64 }
#[inline(never)]
fn k214() -> u64 { 0xd4d4d4d4d4d4d4d4u64 }
#[inline(never)]
fn k215() -> u64 { 0xd5d5d5d5d5d5d5d5u64 }
#[inline(never)]
fn k216() -> u64 { 0xd6d6d6d6d6d6d6d6u64 }
#[inline(never)]
fn k217() -> u64 { 0xd7d7d7d7d7d7d7d7u64 }
#[inline(never)]
fn k218() -> u64 { 0xd8d8d8d8d8d8d8d8u64 }
#[inline(never)]
fn k219() -> u64 { 0xd9d9d9d9d9d9d9d9u64 }
#[inline(never)]
fn k220() -> u64 { 0xdadadadadadadadau64 }
#[inline(never)]
fn k221() -> u64 { 0xdbdbdbdbdbdbdbdbu64 }
#[inline(never)]
fn k222() -> u64 { 0xdcdcdcdcdcdcdcdcu64 }
#[inline(never)]
fn k223() -> u64 { 0xddddddddddddddddu64 }
#[inline(never)]
fn k224() -> u64 { 0xdedededededededeu64 }
#[inline(never)]
fn k225() -> u64 { 0xdfdfdfdfdfdfdfdfu64 }
#[inline(never)]
fn k226() -> u64 { 0xe0e0e0e0e0e0e0e0u64 }
#[inline(never)]
fn k227() -> u64 { 0xe1e1e1e1e1e1e1e1u64 }
#[inline(never)]
fn k228() -> u64 { 0xe2e2e2e2e2e2e2e2u64 }
#[inline(never)]
fn k229() -> u64 { 0xe3e3e3e3e3e3e3e3u64 }
#[inline(never)]
fn k230() -> u64 { 0xe4e4e4e4e4e4e4e4u64 }
#[inline(never)]
fn k231() -> u64 { 0xe5e5e5e5e5e5e5e5u64 }
#[inline(never)]
fn k232() -> u64 { 0xe6e6e6e6e6e6e6e6u64 }
#[inline(never)]
fn k233() -> u64 { 0xe7e7e7e7e7e7e7e7u64 }
#[inline(never)]
fn k234() -> u64 { 0xe8e8e8e8e8e8e8e8u64 }
#[inline(never)]
fn k235() -> u64 { 0xe9e9e9e9e9e9e9e9u64 }
#[inline(never)]
fn k236() -> u64 { 0xeaeaeaeaeaeaeaeau64 }
#[inline(never)]
fn k237() -> u64 { 0xebebebebebebebebu64 }
#[inline(never)]
fn k238() -> u64 { 0xececececececececu64 }
#[inline(never)]
fn k239() -> u64 { 0xededededededededu64 }
#[inline(never)]
fn k240() -> u64 { 0xeeeeeeeeeeeeeeeeu64 }
#[inline(never)]
fn k241() -> u64 { 0xefefefefefefefefu64 }
#[inline(never)]
fn k242() -> u64 { 0xf0f0f0f0f0f0f0f0u64 }
#[inline(never)]
fn k243() -> u64 { 0xf1f1f1f1f1f1f1f1u64 }
#[inline(never)]
fn k244() -> u64 { 0xf2f2f2f2f2f2f2f2u64 }
#[inline(never)]
fn k245() -> u64 { 0xf3f3f3f3f3f3f3f3u64 }
#[inline(never)]
fn k246() -> u64 { 0xf4f4f4f4f4f4f4f4u64 }
#[inline(never)]
fn k247() -> u64 { 0xf5f5f5f5f5f5f5f5u64 }
#[inline(never)]
fn k248() -> u64 { 0xf6f6f6f6f6f6f6f6u64 }
#[inline(never)]
fn k249() -> u64 { 0xf7f7f7f7f7f7f7f7u64 }
#[inline(never)]
fn k250() -> u64 { 0xf8f8f8f8f8f8f8f8u64 }
#[inline(never)]
fn k251() -> u64 { 0xf9f9f9f9f9f9f9f9u64 }
#[inline(never)]
fn k252() -> u64 { 0xfafafafafafafafau64 }
#[inline(never)]
fn k253() -> u64 { 0xfbfbfbfbfbfbfbfbu64 }
#[inline(never)]
fn k254() -> u64 { 0xfcfcfcfcfcfcfcfcu64 }
#[inline(never)]
fn k255() -> u64 { 0xfdfdfdfdfdfdfdfdu64 }
#[inline(never)]
fn k256() -> u64 { 0xfefefefefefefefeu64 }
#[inline(never)]
fn k257() -> u64 { 0xffffffffffffffffu64 }
#[inline(never)]
fn k258() -> u64 { 0x5c5c5c5c5c5c5c06u64 }
#[inline(never)]
fn k259() -> u64 { 0x5c5c5c5c065c5c5cu64 }
#[inline(never)]
fn k260() -> u64 { 0x065c5c5c5c5c5c5cu64 }
#[inline(never)]
fn k261() -> u64 { 0xfefefefefefefea4u64 }
#[inline(never)]
fn k262() -> u64 { 0xfefefefea4fefefeu64 }
#[inline(never)]
fn k263() -> u64 { 0xa4fefefefefefefeu64 }
#[inline(never)]
fn k264() -> u64 { 0x010101010101015bu64 }
#[inline(never)]
fn k265() -> u64 { 0x010101015b010101u64 }
#[inline(never)]
fn k266() -> u64 { 0x5b01010101010101u64 }
#[inline(never)]
fn k267() -> u64 { 0x80808080808080dau64 }
#[inline(never)]
fn k268() -> u64 { 0x80808080da808080u64 }
#[inline(never)]
fn k269() -> u64 { 0xda80808080808080u64 }
#[inline(never)]
fn k270() -> u64 { 0x7f7f7f7f7f7f7f25u64 }
#[inline(never)]
fn k271() -> u64 { 0x7f7f7f7f257f7f7fu64 }
#[inline(never)]
fn k272() -> u64 { 0x257f7f7f7f7f7f7fu64 }
#[inline(never)]
fn k273() -> u64 { 0xaaaaaaaaaaaaaaf0u64 }
#[inline(never)]
fn k274() -> u64 { 0xaaaaaaaaf0aaaaaau64 }
#[inline(never)]
fn k275() -> u64 { 0xf0aaaaaaaaaaaaaau64 }
#[inline(never)]
fn k276() -> u64 { 0x363636363636366cu64 }
#[inline(never)]
fn k277() -> u64 { 0x363636366c363636u64 }
#[inline(never)]
fn k278() -> u64 { 0x6c36363636363636u64 }
#[inline(never)]
fn k279() -> u64 { 0x0000000000000001u64 }
#[inline(never)]
fn k280() -> u64 { 0xfffffffffffffffeu64 }
#[inline(never)]
fn k281() -> u64 { 0x0000000000000100u64 }
#[inline(never)]
fn k282() -> u64 { 0xfffffffffffffeffu64 }
#[inline(never)]
fn k283() -> u64 { 0x0000000000010000u64 }
#[inline(never)]
fn k284() -> u64 { 0xfffffffffffeffffu64 }
#[inline(never)]
fn k285() -> u64 { 0x0000000080000000u64 }
#[inline(never)]
fn k286() -> u64 { 0xffffffff7fffffffu64 }
#[inline(never)]
fn k287() -> u64 { 0x000000000000007fu64 }
#[inline(never)]
fn k288() -> u64 { 0xffffffffffffff80u64 }
#[inline(never)]
fn k289() -> u64 { 0x0000000000007f00u64 }
#[inline(never)]
fn k290() -> u64 { 0xffffffffffff80ffu64 }
#[inline(never)]
fn k291() -> u64 { 0x00000000007f0000u64 }
#[inline(never)]
fn k292() -> u64 { 0xffffffffff80ffffu64 }
#[inline(never)]
fn k293() -> u64 { 0x0000003f80000000u64 }
#[inline(never)]
fn k294() -> u64 { 0xffffffc07fffffffu64 }
#[inline(never)]
fn k295() -> u64 { 0x00000000000000ffu64 }
#[inline(never)]
fn k296() -> u64 { 0xffffffffffffff00u64 }
#[inline(never)]
fn k297() -> u64 { 0x000000000000ff00u64 }
#[inline(never)]
fn k298() -> u64 { 0xffffffffffff00ffu64 }
#[inline(never)]
fn k299() -> u64 { 0x0000000000ff0000u64 }
#[inline(never)]
fn k300() -> u64 { 0xffffffffff00ffffu64 }
#[inline(never)]
fn k301() -> u64 { 0x0000007f80000000u64 }
#[inline(never)]
fn k302() -> u64 { 0xffffff807fffffffu64 }
#[inline(never)]
fn k303() -> u64 { 0x000000000000ffffu64 }
#[inline(never)]
fn k304() -> u64 { 0xffffffffffff0000u64 }
#[inline(never)]
fn k305() -> u64 { 0x0000000000ffff00u64 }
#[inline(never)]
fn k306() -> u64 { 0xffffffffff0000ffu64 }
#[inline(never)]
fn k307() -> u64 { 0x00000000ffff0000u64 }
#[inline(never)]
fn k308() -> u64 { 0xffffffff0000ffffu64 }
#[inline(never)]
fn k309() -> u64 { 0x00007fff80000000u64 }
#[inline(never)]
fn k310() -> u64 { 0xffff80007fffffffu64 }
#[inline(never)]
fn k311() -> u64 { 0x00000000ffffffffu64 }
#[inline(never)]
fn k312() -> u64 { 0xffffffff00000000u64 }
#[inline(never)]
fn k313() -> u64 { 0x000000ffffffff00u64 }
#[inline(never)]
fn k314() -> u64 { 0xffffff00000000ffu64 }
#[inline(never)]
fn k315() -> u64 { 0x0000ffffffff0000u64 }
#[inline(never)]
fn k316() -> u64 { 0xffff00000000ffffu64 }
#[inline(never)]
fn k317() -> u64 { 0x7fffffff80000000u64 }
#[inline(never)]
fn k318() -> u64 { 0x800000007fffffffu64 }
#[inline(never)]
fn k319() -> u64 { 0x0000ffffffffffffu64 }
#[inline(never)]
fn k320() -> u64 { 0xffff000000000000u64 }
#[inline(never)]
fn k321() -> u64 { 0x00ffffffffffff00u64 }
#[inline(never)]
fn k322() -> u64 { 0xff000000000000ffu64 }
#[inline(never)]
fn k323() -> u64 { 0x7fffffffffffffffu64 }
#[inline(never)]
fn k324() -> u64 { 0x8000000000000000u64 }
#[inline(never)]
fn k325() -> u64 { 0xdeadbeefdeadbeefu64 }
#[inline(never)]
fn k326() -> u64 { 0x1234567812345678u64 }
#[inline(never)]
fn k327() -> u64 { 0xabcdef01abcdef01u64 }
#[inline(never)]
fn k328() -> u64 { 0xffff0000ffff0000u64 }
#[inline(never)]
fn k329() -> u64 { 0x0000ffff0000ffffu64 }
#[inline(never)]
fn k330() -> u64 { 0x1c80317fa3b1799du64 }
#[inline(never)]
fn k331() -> u64 { 0xbdd640fb06671ad1u64 }
#[inline(never)]
fn k332() -> u64 { 0x3eb13b9046685257u64 }
#[inline(never)]
fn k333() -> u64 { 0x23b8c1e9392456deu64 }
#[inline(never)]
fn k334() -> u64 { 0x1a3d1fa7bc8960a9u64 }
#[inline(never)]
fn k335() -> u64 { 0xbd9c66b3ad3c2d6du64 }
#[inline(never)]
fn k336() -> u64 { 0x8b9d2434e465e150u64 }
#[inline(never)]
fn k337() -> u64 { 0x972a846916419f82u64 }
#[inline(never)]
fn k338() -> u64 { 0x0822e8f36c031199u64 }
#[inline(never)]
fn k339() -> u64 { 0x17fc695a07a0ca6eu64 }
#[inline(never)]
fn k340() -> u64 { 0x3b8faa1837f8a88bu64 }
#[inline(never)]
fn k341() -> u64 { 0x9a1de644815ef6d1u64 }
#[inline(never)]
fn k342() -> u64 { 0x8fadc1a606cb0fb3u64 }
#[inline(never)]
fn k343() -> u64 { 0xb74d0fb132e70629u64 }
#[inline(never)]
fn k344() -> u64 { 0xb38a088ca65ed389u64 }
#[inline(never)]
fn k345() -> u64 { 0x6b65a6a48b8148f6u64 }
#[inline(never)]
fn k346() -> u64 { 0x72ff5d2a386ecbe0u64 }
#[inline(never)]
fn k347() -> u64 { 0x4737819096da1dacu64 }
#[inline(never)]
fn k348() -> u64 { 0xde8a774bcf36d58bu64 }
#[inline(never)]
fn k349() -> u64 { 0xc241330b01a9e71fu64 }
#[inline(never)]
fn k350() -> u64 { 0x28df6ec4ce4a2bbdu64 }
#[inline(never)]
fn k351() -> u64 { 0x6c307511b2b9437au64 }
#[inline(never)]
fn k352() -> u64 { 0x47229389571aa876u64 }
#[inline(never)]
fn k353() -> u64 { 0x371ecd7b27cd8130u64 }
#[inline(never)]
fn k354() -> u64 { 0xc37459eef50bea63u64 }
#[inline(never)]
fn k355() -> u64 { 0x1a2a73ed562b0f79u64 }
#[inline(never)]
fn k356() -> u64 { 0x6142ea7d17be3111u64 }
#[inline(never)]
fn k357() -> u64 { 0x5be6128e18c26797u64 }
#[inline(never)]
fn k358() -> u64 { 0x580d7b71d8f56413u64 }
#[inline(never)]
fn k359() -> u64 { 0x43b7a3a69a8dca03u64 }
#[inline(never)]
fn k360() -> u64 { 0x0b1f9163ce9ff57fu64 }
#[inline(never)]
fn k361() -> u64 { 0x759cde66bacfb3d0u64 }
#[inline(never)]
fn k362() -> u64 { 0x1ff49b7889463e85u64 }
#[inline(never)]
fn k363() -> u64 { 0xec1b8ca1f91e1d4cu64 }
#[inline(never)]
fn k364() -> u64 { 0x142c3fe860e7a113u64 }
#[inline(never)]
fn k365() -> u64 { 0x4b0dbb418d5288f1u64 }
#[inline(never)]
fn k366() -> u64 { 0xa0ee89aed453dd32u64 }
#[inline(never)]
fn k367() -> u64 { 0xe2acf72f9e574f7au64 }
#[inline(never)]
fn k368() -> u64 { 0x5c941cf0dc98d2c1u64 }
#[inline(never)]
fn k369() -> u64 { 0x3139d32c93cd59bfu64 }
#[inline(never)]
fn k370() -> u64 { 0x11ce5dd2b45ed1f0u64 }
#[inline(never)]
fn k371() -> u64 { 0xa9488d990bbb2599u64 }
#[inline(never)]
fn k372() -> u64 { 0xc5e7ce8a3a578a8eu64 }
#[inline(never)]
fn k373() -> u64 { 0xfc377a4c4a15544du64 }
#[inline(never)]
fn k374() -> u64 { 0xdaf61a26146d3f31u64 }
#[inline(never)]
fn k375() -> u64 { 0xddd1dfb23b982ef8u64 }
#[inline(never)]
fn k376() -> u64 { 0x614ff3d719db3ad0u64 }
#[inline(never)]
fn k377() -> u64 { 0x7412b29347294739u64 }
#[inline(never)]
fn k378() -> u64 { 0xd58842dea2bc372fu64 }
#[inline(never)]
fn k379() -> u64 { 0x29a3b2e95d65a441u64 }
#[inline(never)]
fn k380() -> u64 { 0x5af305535ec42e08u64 }
#[inline(never)]
fn k381() -> u64 { 0xab9099a435a240aeu64 }
#[inline(never)]
fn k382() -> u64 { 0xb3aa7efe4458a885u64 }
#[inline(never)]
fn k383() -> u64 { 0xaefcfad8efc89849u64 }
#[inline(never)]
fn k384() -> u64 { 0x12476f57a5e5a5abu64 }
#[inline(never)]
fn k385() -> u64 { 0xa28defe39bf00273u64 }
#[inline(never)]
fn k386() -> u64 { 0x88bd64072bcfbe01u64 }
#[inline(never)]
fn k387() -> u64 { 0x3eabedcbbaa80dd4u64 }
#[inline(never)]
fn k388() -> u64 { 0x7656af7229d4beefu64 }
#[inline(never)]
fn k389() -> u64 { 0x451b4cf36123fdf7u64 }
#[inline(never)]
fn k390() -> u64 { 0xece66fa2fd5166e6u64 }
#[inline(never)]
fn k391() -> u64 { 0xb02b61c4a3d70628u64 }
#[inline(never)]
fn k392() -> u64 { 0x3838b3268e944239u64 }
#[inline(never)]
fn k393() -> u64 { 0x5304317faf42e12fu64 }
#[inline(never)]
fn k394() -> u64 { 0xc4b032ccd7c524a5u64 }
#[inline(never)]
fn k395() -> u64 { 0x0e51f30dc6a7ee39u64 }
#[inline(never)]
fn k396() -> u64 { 0xd261a7ab3aa2e4f9u64 }
#[inline(never)]
fn k397() -> u64 { 0xce177b4e0837b8a3u64 }
#[inline(never)]
fn k398() -> u64 { 0x66b2bc5b50c187fcu64 }
#[inline(never)]
fn k399() -> u64 { 0x10f1bc81448aaa9eu64 }
#[inline(never)]
fn k400() -> u64 { 0xe9c349e03602f8acu64 }
#[inline(never)]
fn k401() -> u64 { 0x9132b63ef16287e4u64 }
#[inline(never)]
fn k402() -> u64 { 0xb7c93acfe059a0eeu64 }
#[inline(never)]
fn k403() -> u64 { 0x366eb16f508ebad7u64 }
#[inline(never)]
fn k404() -> u64 { 0x7fcd9eb1a7cad415u64 }
#[inline(never)]
fn k405() -> u64 { 0xe27a984d654821d0u64 }
#[inline(never)]
fn k406() -> u64 { 0xa491f0b2ea1fca65u64 }
#[inline(never)]
fn k407() -> u64 { 0x24933b83757750a9u64 }
#[inline(never)]
fn k408() -> u64 { 0x23bed01d43cf2fdeu64 }
#[inline(never)]
fn k409() -> u64 { 0xbeb799193f22faf8u64 }
#[inline(never)]
fn k410() -> u64 { 0x89fa6a688fb5d27bu64 }
#[inline(never)]
fn k411() -> u64 { 0xbf3c4c06434308bcu64 }
#[inline(never)]
fn k412() -> u64 { 0x6dadd6c795a76d79u64 }
#[inline(never)]
fn k413() -> u64 { 0x956269f0e5d7b875u64 }
#[inline(never)]
fn k414() -> u64 { 0x5cabcc97663f1c97u64 }
#[inline(never)]
fn k415() -> u64 { 0xff50bde4382567b8u64 }
#[inline(never)]
fn k416() -> u64 { 0x2369b584ff5e9ff0u64 }
#[inline(never)]
fn k417() -> u64 { 0x7e570ddf827050a8u64 }
#[inline(never)]
fn k418() -> u64 { 0xc17af08a1745d6d8u64 }
#[inline(never)]
fn k419() -> u64 { 0xdc713d960c0fd195u64 }
#[inline(never)]
fn k420() -> u64 { 0x27209bdf1c11f735u64 }
#[inline(never)]
fn k421() -> u64 { 0x28f49481a0a04dc4u64 }
#[inline(never)]
fn k422() -> u64 { 0xae340454cac5b68cu64 }
#[inline(never)]
fn k423() -> u64 { 0x98ae43346c12ace8u64 }
#[inline(never)]
fn k424() -> u64 { 0x62801c4510435a10u64 }
#[inline(never)]
fn k425() -> u64 { 0x988c24c961b1cd22u64 }
#[inline(never)]
fn k426() -> u64 { 0x77d21e02ff01cf99u64 }
#[inline(never)]
fn k427() -> u64 { 0x405cacec877409a9u64 }
#[inline(never)]
fn k428() -> u64 { 0x8da0365bf89897b9u64 }
#[inline(never)]
fn k429() -> u64 { 0xf143262fdc5c0eedu64 }
#[inline(never)]
fn k430() -> u64 { 0xae270da702f06b90u64 }
#[inline(never)]
fn k431() -> u64 { 0x1d53434bb88139b9u64 }
#[inline(never)]
fn k432() -> u64 { 0xe2817efdae849217u64 }
#[inline(never)]
fn k433() -> u64 { 0xc03987108976e334u64 }
#[inline(never)]
fn k434() -> u64 { 0xc4c2e2e3444ea7c8u64 }
#[inline(never)]
fn k435() -> u64 { 0x5715bd6fa4161293u64 }
#[inline(never)]
fn k436() -> u64 { 0x4b22d3081c8eaee9u64 }
#[inline(never)]
fn k437() -> u64 { 0x287d06ca6f4cc69au64 }
#[inline(never)]
fn k438() -> u64 { 0x00d4af5974273ca3u64 }
#[inline(never)]
fn k439() -> u64 { 0xb8db0672f42d47ccu64 }
#[inline(never)]
fn k440() -> u64 { 0xb83cfe0be037e5edu64 }
#[inline(never)]
fn k441() -> u64 { 0xf8cda88b436d76e2u64 }
#[inline(never)]
fn k442() -> u64 { 0xc30ff46e8026695fu64 }
#[inline(never)]
fn k443() -> u64 { 0x81f76d1c2dbc2134u64 }
#[inline(never)]
fn k444() -> u64 { 0x1b3dbd5ce9a1fa6fu64 }
#[inline(never)]
fn k445() -> u64 { 0xa013ac6ededa4e16u64 }
#[inline(never)]
fn k446() -> u64 { 0xd777a4774c66e0a8u64 }
#[inline(never)]
fn k447() -> u64 { 0x81f631d4a39231a7u64 }
#[inline(never)]
fn k448() -> u64 { 0x32ebd6899be578c7u64 }
#[inline(never)]
fn k449() -> u64 { 0x5fb8d16c2720797du64 }
#[inline(never)]
fn k450() -> u64 { 0x00000000c333e861u64 }
#[inline(never)]
fn k451() -> u64 { 0x00000000295b4715u64 }
#[inline(never)]
fn k452() -> u64 { 0x000000008a14be62u64 }
#[inline(never)]
fn k453() -> u64 { 0x00000000f4188f3fu64 }
#[inline(never)]
fn k454() -> u64 { 0x00000000c754108fu64 }
#[inline(never)]
fn k455() -> u64 { 0x00000000ec24a3c5u64 }
#[inline(never)]
fn k456() -> u64 { 0x0000000087c5421eu64 }
#[inline(never)]
fn k457() -> u64 { 0x00000000eb2263ddu64 }
#[inline(never)]
fn k458() -> u64 { 0x0000000000257ad1u64 }
#[inline(never)]
fn k459() -> u64 { 0x0000000099546eb4u64 }
#[inline(never)]
fn k460() -> u64 { 0x0000000052fbe43bu64 }
#[inline(never)]
fn k461() -> u64 { 0x000000007d154385u64 }
#[inline(never)]
fn k462() -> u64 { 0x0000000004fc6d82u64 }
#[inline(never)]
fn k463() -> u64 { 0x000000001ca35cfbu64 }
#[inline(never)]
fn k464() -> u64 { 0x00000000edd96831u64 }
#[inline(never)]
fn k465() -> u64 { 0x000000005cec4eb5u64 }
#[inline(never)]
fn k466() -> u64 { 0x00000000e0f3eab0u64 }
#[inline(never)]
fn k467() -> u64 { 0x00000000fc3e058bu64 }
#[inline(never)]
fn k468() -> u64 { 0x00000000d4e80839u64 }
#[inline(never)]
fn k469() -> u64 { 0x00000000ce88cb2du64 }
#[inline(never)]
fn k470() -> u64 { 0x000000000000002au64 }
#[inline(never)]
fn k471() -> u64 { 0x0000000100000000u64 }
#[inline(never)]
fn n_i8() -> i8 { -5i8 }
#[inline(never)]
fn n_i16() -> i16 { -25061i16 }
#[inline(never)]
fn n_u16() -> u16 { 0x9c9cu16 }
#[inline(never)]
fn n_i32() -> i32 { 0x5c5c5c5cu32 as i32 }

fn main() {
    println!("{:016x}", black_box(k0()));
    println!("{:016x}", black_box(k1()));
    println!("{:016x}", black_box(k2()));
    println!("{:016x}", black_box(k3()));
    println!("{:016x}", black_box(k4()));
    println!("{:016x}", black_box(k5()));
    println!("{:016x}", black_box(k6()));
    println!("{:016x}", black_box(k7()));
    println!("{:016x}", black_box(k8()));
    println!("{:016x}", black_box(k9()));
    println!("{:016x}", black_box(k10()));
    println!("{:016x}", black_box(k11()));
    println!("{:016x}", black_box(k12()));
    println!("{:016x}", black_box(k13()));
    println!("{:016x}", black_box(k14()));
    println!("{:016x}", black_box(k15()));
    println!("{:016x}", black_box(k16()));
    println!("{:016x}", black_box(k17()));
    println!("{:016x}", black_box(k18()));
    println!("{:016x}", black_box(k19()));
    println!("{:016x}", black_box(k20()));
    println!("{:016x}", black_box(k21()));
    println!("{:016x}", black_box(k22()));
    println!("{:016x}", black_box(k23()));
    println!("{:016x}", black_box(k24()));
    println!("{:016x}", black_box(k25()));
    println!("{:016x}", black_box(k26()));
    println!("{:016x}", black_box(k27()));
    println!("{:016x}", black_box(k28()));
    println!("{:016x}", black_box(k29()));
    println!("{:016x}", black_box(k30()));
    println!("{:016x}", black_box(k31()));
    println!("{:016x}", black_box(k32()));
    println!("{:016x}", black_box(k33()));
    println!("{:016x}", black_box(k34()));
    println!("{:016x}", black_box(k35()));
    println!("{:016x}", black_box(k36()));
    println!("{:016x}", black_box(k37()));
    println!("{:016x}", black_box(k38()));
    println!("{:016x}", black_box(k39()));
    println!("{:016x}", black_box(k40()));
    println!("{:016x}", black_box(k41()));
    println!("{:016x}", black_box(k42()));
    println!("{:016x}", black_box(k43()));
    println!("{:016x}", black_box(k44()));
    println!("{:016x}", black_box(k45()));
    println!("{:016x}", black_box(k46()));
    println!("{:016x}", black_box(k47()));
    println!("{:016x}", black_box(k48()));
    println!("{:016x}", black_box(k49()));
    println!("{:016x}", black_box(k50()));
    println!("{:016x}", black_box(k51()));
    println!("{:016x}", black_box(k52()));
    println!("{:016x}", black_box(k53()));
    println!("{:016x}", black_box(k54()));
    println!("{:016x}", black_box(k55()));
    println!("{:016x}", black_box(k56()));
    println!("{:016x}", black_box(k57()));
    println!("{:016x}", black_box(k58()));
    println!("{:016x}", black_box(k59()));
    println!("{:016x}", black_box(k60()));
    println!("{:016x}", black_box(k61()));
    println!("{:016x}", black_box(k62()));
    println!("{:016x}", black_box(k63()));
    println!("{:016x}", black_box(k64()));
    println!("{:016x}", black_box(k65()));
    println!("{:016x}", black_box(k66()));
    println!("{:016x}", black_box(k67()));
    println!("{:016x}", black_box(k68()));
    println!("{:016x}", black_box(k69()));
    println!("{:016x}", black_box(k70()));
    println!("{:016x}", black_box(k71()));
    println!("{:016x}", black_box(k72()));
    println!("{:016x}", black_box(k73()));
    println!("{:016x}", black_box(k74()));
    println!("{:016x}", black_box(k75()));
    println!("{:016x}", black_box(k76()));
    println!("{:016x}", black_box(k77()));
    println!("{:016x}", black_box(k78()));
    println!("{:016x}", black_box(k79()));
    println!("{:016x}", black_box(k80()));
    println!("{:016x}", black_box(k81()));
    println!("{:016x}", black_box(k82()));
    println!("{:016x}", black_box(k83()));
    println!("{:016x}", black_box(k84()));
    println!("{:016x}", black_box(k85()));
    println!("{:016x}", black_box(k86()));
    println!("{:016x}", black_box(k87()));
    println!("{:016x}", black_box(k88()));
    println!("{:016x}", black_box(k89()));
    println!("{:016x}", black_box(k90()));
    println!("{:016x}", black_box(k91()));
    println!("{:016x}", black_box(k92()));
    println!("{:016x}", black_box(k93()));
    println!("{:016x}", black_box(k94()));
    println!("{:016x}", black_box(k95()));
    println!("{:016x}", black_box(k96()));
    println!("{:016x}", black_box(k97()));
    println!("{:016x}", black_box(k98()));
    println!("{:016x}", black_box(k99()));
    println!("{:016x}", black_box(k100()));
    println!("{:016x}", black_box(k101()));
    println!("{:016x}", black_box(k102()));
    println!("{:016x}", black_box(k103()));
    println!("{:016x}", black_box(k104()));
    println!("{:016x}", black_box(k105()));
    println!("{:016x}", black_box(k106()));
    println!("{:016x}", black_box(k107()));
    println!("{:016x}", black_box(k108()));
    println!("{:016x}", black_box(k109()));
    println!("{:016x}", black_box(k110()));
    println!("{:016x}", black_box(k111()));
    println!("{:016x}", black_box(k112()));
    println!("{:016x}", black_box(k113()));
    println!("{:016x}", black_box(k114()));
    println!("{:016x}", black_box(k115()));
    println!("{:016x}", black_box(k116()));
    println!("{:016x}", black_box(k117()));
    println!("{:016x}", black_box(k118()));
    println!("{:016x}", black_box(k119()));
    println!("{:016x}", black_box(k120()));
    println!("{:016x}", black_box(k121()));
    println!("{:016x}", black_box(k122()));
    println!("{:016x}", black_box(k123()));
    println!("{:016x}", black_box(k124()));
    println!("{:016x}", black_box(k125()));
    println!("{:016x}", black_box(k126()));
    println!("{:016x}", black_box(k127()));
    println!("{:016x}", black_box(k128()));
    println!("{:016x}", black_box(k129()));
    println!("{:016x}", black_box(k130()));
    println!("{:016x}", black_box(k131()));
    println!("{:016x}", black_box(k132()));
    println!("{:016x}", black_box(k133()));
    println!("{:016x}", black_box(k134()));
    println!("{:016x}", black_box(k135()));
    println!("{:016x}", black_box(k136()));
    println!("{:016x}", black_box(k137()));
    println!("{:016x}", black_box(k138()));
    println!("{:016x}", black_box(k139()));
    println!("{:016x}", black_box(k140()));
    println!("{:016x}", black_box(k141()));
    println!("{:016x}", black_box(k142()));
    println!("{:016x}", black_box(k143()));
    println!("{:016x}", black_box(k144()));
    println!("{:016x}", black_box(k145()));
    println!("{:016x}", black_box(k146()));
    println!("{:016x}", black_box(k147()));
    println!("{:016x}", black_box(k148()));
    println!("{:016x}", black_box(k149()));
    println!("{:016x}", black_box(k150()));
    println!("{:016x}", black_box(k151()));
    println!("{:016x}", black_box(k152()));
    println!("{:016x}", black_box(k153()));
    println!("{:016x}", black_box(k154()));
    println!("{:016x}", black_box(k155()));
    println!("{:016x}", black_box(k156()));
    println!("{:016x}", black_box(k157()));
    println!("{:016x}", black_box(k158()));
    println!("{:016x}", black_box(k159()));
    println!("{:016x}", black_box(k160()));
    println!("{:016x}", black_box(k161()));
    println!("{:016x}", black_box(k162()));
    println!("{:016x}", black_box(k163()));
    println!("{:016x}", black_box(k164()));
    println!("{:016x}", black_box(k165()));
    println!("{:016x}", black_box(k166()));
    println!("{:016x}", black_box(k167()));
    println!("{:016x}", black_box(k168()));
    println!("{:016x}", black_box(k169()));
    println!("{:016x}", black_box(k170()));
    println!("{:016x}", black_box(k171()));
    println!("{:016x}", black_box(k172()));
    println!("{:016x}", black_box(k173()));
    println!("{:016x}", black_box(k174()));
    println!("{:016x}", black_box(k175()));
    println!("{:016x}", black_box(k176()));
    println!("{:016x}", black_box(k177()));
    println!("{:016x}", black_box(k178()));
    println!("{:016x}", black_box(k179()));
    println!("{:016x}", black_box(k180()));
    println!("{:016x}", black_box(k181()));
    println!("{:016x}", black_box(k182()));
    println!("{:016x}", black_box(k183()));
    println!("{:016x}", black_box(k184()));
    println!("{:016x}", black_box(k185()));
    println!("{:016x}", black_box(k186()));
    println!("{:016x}", black_box(k187()));
    println!("{:016x}", black_box(k188()));
    println!("{:016x}", black_box(k189()));
    println!("{:016x}", black_box(k190()));
    println!("{:016x}", black_box(k191()));
    println!("{:016x}", black_box(k192()));
    println!("{:016x}", black_box(k193()));
    println!("{:016x}", black_box(k194()));
    println!("{:016x}", black_box(k195()));
    println!("{:016x}", black_box(k196()));
    println!("{:016x}", black_box(k197()));
    println!("{:016x}", black_box(k198()));
    println!("{:016x}", black_box(k199()));
    println!("{:016x}", black_box(k200()));
    println!("{:016x}", black_box(k201()));
    println!("{:016x}", black_box(k202()));
    println!("{:016x}", black_box(k203()));
    println!("{:016x}", black_box(k204()));
    println!("{:016x}", black_box(k205()));
    println!("{:016x}", black_box(k206()));
    println!("{:016x}", black_box(k207()));
    println!("{:016x}", black_box(k208()));
    println!("{:016x}", black_box(k209()));
    println!("{:016x}", black_box(k210()));
    println!("{:016x}", black_box(k211()));
    println!("{:016x}", black_box(k212()));
    println!("{:016x}", black_box(k213()));
    println!("{:016x}", black_box(k214()));
    println!("{:016x}", black_box(k215()));
    println!("{:016x}", black_box(k216()));
    println!("{:016x}", black_box(k217()));
    println!("{:016x}", black_box(k218()));
    println!("{:016x}", black_box(k219()));
    println!("{:016x}", black_box(k220()));
    println!("{:016x}", black_box(k221()));
    println!("{:016x}", black_box(k222()));
    println!("{:016x}", black_box(k223()));
    println!("{:016x}", black_box(k224()));
    println!("{:016x}", black_box(k225()));
    println!("{:016x}", black_box(k226()));
    println!("{:016x}", black_box(k227()));
    println!("{:016x}", black_box(k228()));
    println!("{:016x}", black_box(k229()));
    println!("{:016x}", black_box(k230()));
    println!("{:016x}", black_box(k231()));
    println!("{:016x}", black_box(k232()));
    println!("{:016x}", black_box(k233()));
    println!("{:016x}", black_box(k234()));
    println!("{:016x}", black_box(k235()));
    println!("{:016x}", black_box(k236()));
    println!("{:016x}", black_box(k237()));
    println!("{:016x}", black_box(k238()));
    println!("{:016x}", black_box(k239()));
    println!("{:016x}", black_box(k240()));
    println!("{:016x}", black_box(k241()));
    println!("{:016x}", black_box(k242()));
    println!("{:016x}", black_box(k243()));
    println!("{:016x}", black_box(k244()));
    println!("{:016x}", black_box(k245()));
    println!("{:016x}", black_box(k246()));
    println!("{:016x}", black_box(k247()));
    println!("{:016x}", black_box(k248()));
    println!("{:016x}", black_box(k249()));
    println!("{:016x}", black_box(k250()));
    println!("{:016x}", black_box(k251()));
    println!("{:016x}", black_box(k252()));
    println!("{:016x}", black_box(k253()));
    println!("{:016x}", black_box(k254()));
    println!("{:016x}", black_box(k255()));
    println!("{:016x}", black_box(k256()));
    println!("{:016x}", black_box(k257()));
    println!("{:016x}", black_box(k258()));
    println!("{:016x}", black_box(k259()));
    println!("{:016x}", black_box(k260()));
    println!("{:016x}", black_box(k261()));
    println!("{:016x}", black_box(k262()));
    println!("{:016x}", black_box(k263()));
    println!("{:016x}", black_box(k264()));
    println!("{:016x}", black_box(k265()));
    println!("{:016x}", black_box(k266()));
    println!("{:016x}", black_box(k267()));
    println!("{:016x}", black_box(k268()));
    println!("{:016x}", black_box(k269()));
    println!("{:016x}", black_box(k270()));
    println!("{:016x}", black_box(k271()));
    println!("{:016x}", black_box(k272()));
    println!("{:016x}", black_box(k273()));
    println!("{:016x}", black_box(k274()));
    println!("{:016x}", black_box(k275()));
    println!("{:016x}", black_box(k276()));
    println!("{:016x}", black_box(k277()));
    println!("{:016x}", black_box(k278()));
    println!("{:016x}", black_box(k279()));
    println!("{:016x}", black_box(k280()));
    println!("{:016x}", black_box(k281()));
    println!("{:016x}", black_box(k282()));
    println!("{:016x}", black_box(k283()));
    println!("{:016x}", black_box(k284()));
    println!("{:016x}", black_box(k285()));
    println!("{:016x}", black_box(k286()));
    println!("{:016x}", black_box(k287()));
    println!("{:016x}", black_box(k288()));
    println!("{:016x}", black_box(k289()));
    println!("{:016x}", black_box(k290()));
    println!("{:016x}", black_box(k291()));
    println!("{:016x}", black_box(k292()));
    println!("{:016x}", black_box(k293()));
    println!("{:016x}", black_box(k294()));
    println!("{:016x}", black_box(k295()));
    println!("{:016x}", black_box(k296()));
    println!("{:016x}", black_box(k297()));
    println!("{:016x}", black_box(k298()));
    println!("{:016x}", black_box(k299()));
    println!("{:016x}", black_box(k300()));
    println!("{:016x}", black_box(k301()));
    println!("{:016x}", black_box(k302()));
    println!("{:016x}", black_box(k303()));
    println!("{:016x}", black_box(k304()));
    println!("{:016x}", black_box(k305()));
    println!("{:016x}", black_box(k306()));
    println!("{:016x}", black_box(k307()));
    println!("{:016x}", black_box(k308()));
    println!("{:016x}", black_box(k309()));
    println!("{:016x}", black_box(k310()));
    println!("{:016x}", black_box(k311()));
    println!("{:016x}", black_box(k312()));
    println!("{:016x}", black_box(k313()));
    println!("{:016x}", black_box(k314()));
    println!("{:016x}", black_box(k315()));
    println!("{:016x}", black_box(k316()));
    println!("{:016x}", black_box(k317()));
    println!("{:016x}", black_box(k318()));
    println!("{:016x}", black_box(k319()));
    println!("{:016x}", black_box(k320()));
    println!("{:016x}", black_box(k321()));
    println!("{:016x}", black_box(k322()));
    println!("{:016x}", black_box(k323()));
    println!("{:016x}", black_box(k324()));
    println!("{:016x}", black_box(k325()));
    println!("{:016x}", black_box(k326()));
    println!("{:016x}", black_box(k327()));
    println!("{:016x}", black_box(k328()));
    println!("{:016x}", black_box(k329()));
    println!("{:016x}", black_box(k330()));
    println!("{:016x}", black_box(k331()));
    println!("{:016x}", black_box(k332()));
    println!("{:016x}", black_box(k333()));
    println!("{:016x}", black_box(k334()));
    println!("{:016x}", black_box(k335()));
    println!("{:016x}", black_box(k336()));
    println!("{:016x}", black_box(k337()));
    println!("{:016x}", black_box(k338()));
    println!("{:016x}", black_box(k339()));
    println!("{:016x}", black_box(k340()));
    println!("{:016x}", black_box(k341()));
    println!("{:016x}", black_box(k342()));
    println!("{:016x}", black_box(k343()));
    println!("{:016x}", black_box(k344()));
    println!("{:016x}", black_box(k345()));
    println!("{:016x}", black_box(k346()));
    println!("{:016x}", black_box(k347()));
    println!("{:016x}", black_box(k348()));
    println!("{:016x}", black_box(k349()));
    println!("{:016x}", black_box(k350()));
    println!("{:016x}", black_box(k351()));
    println!("{:016x}", black_box(k352()));
    println!("{:016x}", black_box(k353()));
    println!("{:016x}", black_box(k354()));
    println!("{:016x}", black_box(k355()));
    println!("{:016x}", black_box(k356()));
    println!("{:016x}", black_box(k357()));
    println!("{:016x}", black_box(k358()));
    println!("{:016x}", black_box(k359()));
    println!("{:016x}", black_box(k360()));
    println!("{:016x}", black_box(k361()));
    println!("{:016x}", black_box(k362()));
    println!("{:016x}", black_box(k363()));
    println!("{:016x}", black_box(k364()));
    println!("{:016x}", black_box(k365()));
    println!("{:016x}", black_box(k366()));
    println!("{:016x}", black_box(k367()));
    println!("{:016x}", black_box(k368()));
    println!("{:016x}", black_box(k369()));
    println!("{:016x}", black_box(k370()));
    println!("{:016x}", black_box(k371()));
    println!("{:016x}", black_box(k372()));
    println!("{:016x}", black_box(k373()));
    println!("{:016x}", black_box(k374()));
    println!("{:016x}", black_box(k375()));
    println!("{:016x}", black_box(k376()));
    println!("{:016x}", black_box(k377()));
    println!("{:016x}", black_box(k378()));
    println!("{:016x}", black_box(k379()));
    println!("{:016x}", black_box(k380()));
    println!("{:016x}", black_box(k381()));
    println!("{:016x}", black_box(k382()));
    println!("{:016x}", black_box(k383()));
    println!("{:016x}", black_box(k384()));
    println!("{:016x}", black_box(k385()));
    println!("{:016x}", black_box(k386()));
    println!("{:016x}", black_box(k387()));
    println!("{:016x}", black_box(k388()));
    println!("{:016x}", black_box(k389()));
    println!("{:016x}", black_box(k390()));
    println!("{:016x}", black_box(k391()));
    println!("{:016x}", black_box(k392()));
    println!("{:016x}", black_box(k393()));
    println!("{:016x}", black_box(k394()));
    println!("{:016x}", black_box(k395()));
    println!("{:016x}", black_box(k396()));
    println!("{:016x}", black_box(k397()));
    println!("{:016x}", black_box(k398()));
    println!("{:016x}", black_box(k399()));
    println!("{:016x}", black_box(k400()));
    println!("{:016x}", black_box(k401()));
    println!("{:016x}", black_box(k402()));
    println!("{:016x}", black_box(k403()));
    println!("{:016x}", black_box(k404()));
    println!("{:016x}", black_box(k405()));
    println!("{:016x}", black_box(k406()));
    println!("{:016x}", black_box(k407()));
    println!("{:016x}", black_box(k408()));
    println!("{:016x}", black_box(k409()));
    println!("{:016x}", black_box(k410()));
    println!("{:016x}", black_box(k411()));
    println!("{:016x}", black_box(k412()));
    println!("{:016x}", black_box(k413()));
    println!("{:016x}", black_box(k414()));
    println!("{:016x}", black_box(k415()));
    println!("{:016x}", black_box(k416()));
    println!("{:016x}", black_box(k417()));
    println!("{:016x}", black_box(k418()));
    println!("{:016x}", black_box(k419()));
    println!("{:016x}", black_box(k420()));
    println!("{:016x}", black_box(k421()));
    println!("{:016x}", black_box(k422()));
    println!("{:016x}", black_box(k423()));
    println!("{:016x}", black_box(k424()));
    println!("{:016x}", black_box(k425()));
    println!("{:016x}", black_box(k426()));
    println!("{:016x}", black_box(k427()));
    println!("{:016x}", black_box(k428()));
    println!("{:016x}", black_box(k429()));
    println!("{:016x}", black_box(k430()));
    println!("{:016x}", black_box(k431()));
    println!("{:016x}", black_box(k432()));
    println!("{:016x}", black_box(k433()));
    println!("{:016x}", black_box(k434()));
    println!("{:016x}", black_box(k435()));
    println!("{:016x}", black_box(k436()));
    println!("{:016x}", black_box(k437()));
    println!("{:016x}", black_box(k438()));
    println!("{:016x}", black_box(k439()));
    println!("{:016x}", black_box(k440()));
    println!("{:016x}", black_box(k441()));
    println!("{:016x}", black_box(k442()));
    println!("{:016x}", black_box(k443()));
    println!("{:016x}", black_box(k444()));
    println!("{:016x}", black_box(k445()));
    println!("{:016x}", black_box(k446()));
    println!("{:016x}", black_box(k447()));
    println!("{:016x}", black_box(k448()));
    println!("{:016x}", black_box(k449()));
    println!("{:016x}", black_box(k450()));
    println!("{:016x}", black_box(k451()));
    println!("{:016x}", black_box(k452()));
    println!("{:016x}", black_box(k453()));
    println!("{:016x}", black_box(k454()));
    println!("{:016x}", black_box(k455()));
    println!("{:016x}", black_box(k456()));
    println!("{:016x}", black_box(k457()));
    println!("{:016x}", black_box(k458()));
    println!("{:016x}", black_box(k459()));
    println!("{:016x}", black_box(k460()));
    println!("{:016x}", black_box(k461()));
    println!("{:016x}", black_box(k462()));
    println!("{:016x}", black_box(k463()));
    println!("{:016x}", black_box(k464()));
    println!("{:016x}", black_box(k465()));
    println!("{:016x}", black_box(k466()));
    println!("{:016x}", black_box(k467()));
    println!("{:016x}", black_box(k468()));
    println!("{:016x}", black_box(k469()));
    println!("{:016x}", black_box(k470()));
    println!("{:016x}", black_box(k471()));
    println!("{:02x}", black_box(n_i8()));
    println!("{:04x}", black_box(n_i16()));
    println!("{:04x}", black_box(n_u16()));
    println!("{:08x}", black_box(n_i32()));
}
