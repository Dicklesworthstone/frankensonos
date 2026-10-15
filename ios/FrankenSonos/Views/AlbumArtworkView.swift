import SwiftUI

struct AlbumArtworkView: View {
    let track: Track
    var cornerRadius: CGFloat = 8

    var body: some View {
        GeometryReader { geometry in
            let side = geometry.size.width
            ZStack(alignment: .bottomLeading) {
                LinearGradient(colors: track.colors + [Color(red: 0.06, green: 0.10, blue: 0.12)], startPoint: .topLeading, endPoint: .bottomTrailing)
                RadialGradient(colors: [track.colors.last!.opacity(0.8), .clear], center: UnitPoint(x: 0.8, y: 0.2), startRadius: 0, endRadius: side * 0.9)
                Canvas { context, size in
                    let w = size.width
                    let h = size.height
                    switch track.id % 4 {
                    case 1:
                        for index in 0..<7 {
                            let inset = CGFloat(index) * w * 0.052
                            let rect = CGRect(x: w * 0.12 + inset, y: h * 0.12 + inset, width: w * 0.76 - inset * 1.4, height: h * 0.72 - inset * 1.2)
                            context.stroke(Path(roundedRect: rect, cornerRadius: w * 0.35), with: .color(.white.opacity(0.36 - Double(index) * 0.025)), lineWidth: w * 0.018)
                        }
                        context.fill(Path(CGRect(x: w * 0.48, y: 0, width: w * 0.08, height: h)), with: .color(track.colors.last!.opacity(0.55)))
                    case 2:
                        for index in 0..<9 {
                            var path = Path()
                            let y = CGFloat(index) * h * 0.1
                            path.move(to: CGPoint(x: 0, y: y + h * 0.22))
                            path.addCurve(to: CGPoint(x: w, y: y - h * 0.12), control1: CGPoint(x: w * 0.25, y: y - h * 0.28), control2: CGPoint(x: w * 0.7, y: y + h * 0.32))
                            context.stroke(path, with: .color(.white.opacity(0.16 + Double(index % 3) * 0.07)), lineWidth: w * 0.065)
                        }
                    case 3:
                        context.fill(Path(ellipseIn: CGRect(x: w * 0.18, y: h * 0.15, width: w * 0.64, height: w * 0.64)), with: .color(Color(red: 1, green: 0.85, blue: 0.58).opacity(0.78)))
                        for index in 0..<8 {
                            let y = h * 0.5 + CGFloat(index) * h * 0.062
                            context.fill(Path(CGRect(x: 0, y: y, width: w, height: h * 0.024)), with: .color(track.colors.first!.opacity(0.5)))
                        }
                    default:
                        for index in 0..<8 {
                            var path = Path()
                            let x = CGFloat(index) * w * 0.18 - w * 0.2
                            path.move(to: CGPoint(x: x, y: h))
                            path.addLine(to: CGPoint(x: x + w * 0.35, y: h * 0.15))
                            path.addLine(to: CGPoint(x: x + w * 0.5, y: h))
                            path.closeSubpath()
                            context.fill(path, with: .color(index.isMultiple(of: 2) ? .white.opacity(0.18) : .black.opacity(0.16)))
                        }
                    }
                    // Deterministic paper grain, shared at every artwork size.
                    for index in 0..<1600 {
                        let seed = index * 7919 + track.id * 431
                        let x = CGFloat(seed % 997) / 997 * w
                        let y = CGFloat((seed * 37) % 991) / 991 * h
                        context.fill(Path(ellipseIn: CGRect(x: x, y: y, width: max(0.45, w / 400), height: max(0.45, w / 400))), with: .color(.white.opacity(Double(index % 4 + 1) * 0.035)))
                    }
                }
                if side > 90 {
                    VStack(alignment: .leading, spacing: side * 0.016) {
                        Text(track.artist.uppercased()).font(.system(size: side * 0.033, weight: .medium)).tracking(side * 0.008)
                        Text(track.album.uppercased()).font(.system(size: side * 0.055, weight: .semibold)).tracking(side * 0.003)
                    }
                    .foregroundStyle(.white.opacity(0.9))
                    .padding(side * 0.065)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(LinearGradient(colors: [.clear, .black.opacity(0.36)], startPoint: .top, endPoint: .bottom))
                }
                if let url = track.artURL {
                    AsyncImage(url: url) { image in image.resizable().scaledToFill() } placeholder: { Color.clear }
                        .frame(width: geometry.size.width, height: geometry.size.height).clipped()
                }
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
        .shadow(color: .black.opacity(0.13), radius: 4, y: 2)
        .accessibilityLabel("Generated cover for \(track.album)")
    }
}
