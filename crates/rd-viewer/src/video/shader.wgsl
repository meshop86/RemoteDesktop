// Ghép hai plane Y và CbCr thành ảnh màu.
//
// Bộ giải mã trả về video ở không gian màu Y'CbCr dải hẹp (video range): giá trị
// hợp lệ chỉ nằm trong [16,235] chứ không phải [0,255], phần thừa dành cho tín
// hiệu vượt ngưỡng. Hai hệ số `luma`/`chroma` dưới đây kéo dải hẹp đó về [0,1]
// và chuyển chroma về khoảng [-0.5, 0.5]; nhờ vậy một shader dùng được cho cả
// NV12 8-bit lẫn 4:2:2 10-bit, chỉ khác hệ số.

struct Params {
    // x = hệ số nhân, y = độ dời
    luma: vec2<f32>,
    chroma: vec2<f32>,
};

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_cbcr: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> params: Params;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    // Một tam giác duy nhất phủ quá khổ màn hình. Rẻ hơn quad hai tam giác vì
    // GPU không phải xử lý đường chéo nơi hai tam giác giáp nhau.
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);

    var out: VsOut;
    out.uv = vec2<f32>(x, y);
    out.position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let luma = textureSample(tex_y, samp, in.uv).r * params.luma.x + params.luma.y;
    let chroma = textureSample(tex_cbcr, samp, in.uv).rg * params.chroma.x
        + vec2<f32>(params.chroma.y, params.chroma.y);
    let cb = chroma.x;
    let cr = chroma.y;

    // Ma trận BT.709 — chuẩn màu của mọi nội dung HD, và cũng là chuẩn mà
    // VideoToolbox gắn vào luồng khi mã hoá.
    let rgb = vec3<f32>(
        luma + 1.5748 * cr,
        luma - 0.1873 * cb - 0.4681 * cr,
        luma + 1.8556 * cb,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
