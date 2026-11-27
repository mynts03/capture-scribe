import XCTest
@testable import CaptureScribeCapture

final class GeometryTests: XCTestCase {
    let bounds = CGRect(x:0,y:0,width:1000,height:800)
    func testOutsideDragIsInertAndMoveStaysInsideDisplay() {
        let start=CGPoint(x:500,y:400), end=CGPoint(x:200,y:100)
        for (a,b) in [(start,end),(end,start)] {
            let original=CGRect(x:100,y:100,width:200,height:200)
            XCTAssertEqual(dragRegion(original:original,start:a,current:b,handle:-2,bounds:bounds,scale:2),original)
        }
        let r=dragRegion(original:CGRect(x:100,y:100,width:200,height:200),start:.zero,current:CGPoint(x:2000,y:-1000),handle:-1,bounds:bounds,scale:1)
        XCTAssertEqual(r,CGRect(x:800,y:0,width:200,height:200))
    }
    func testAllEightHandlesClampAtEdgesAndKeepOppositeEdges() {
        let original=CGRect(x:200,y:200,width:300,height:300)
        for handle in 0..<8 {
            let r=dragRegion(original:original,start:.zero,current:CGPoint(x:-100,y:-100),handle:handle,bounds:bounds,scale:2)
            XCTAssertTrue(bounds.contains(r)); XCTAssertGreaterThanOrEqual(r.width,16); XCTAssertGreaterThanOrEqual(r.height,16)
            if [0,6,7].contains(handle) { XCTAssertEqual(r.maxX,original.maxX) }
            if [0,1,2].contains(handle) { XCTAssertEqual(r.maxY,original.maxY) }
        }
    }
    func testOversizeClampNeverLeavesNegativeOrigin() {
        for r in [CGRect(x:-100,y:-10,width:2000,height:2000),CGRect(x:999,y:799,width:50,height:50)] {
            let clamped=constrainRegion(r,bounds:bounds)
            XCTAssertTrue(bounds.contains(clamped)); XCTAssertGreaterThanOrEqual(clamped.minX,0)
        }
    }
    func testPointToPixelAlignmentUsesProvidedScaleNotFixedRetinaTwo() {
        for scale in [1.0,1.5,2.0] {
            let r=pixelAligned(CGRect(x:0.2,y:1.2,width:100.3,height:90.3),bounds:bounds,scale:scale)
            XCTAssertEqual(r.minX*scale,(r.minX*scale).rounded(),accuracy:0.00001)
            XCTAssertEqual(r.maxX*scale,(r.maxX*scale).rounded(),accuracy:0.00001)
            XCTAssertTrue(bounds.contains(r))
        }
    }
    func testInvalidAndOutsideRegionsAreRejected() {
        for r in [CGRect(x:CGFloat.nan,y:0,width:30,height:30),CGRect(x:-1,y:0,width:30,height:30),CGRect(x:0,y:0,width:1001,height:30),CGRect(x:0,y:0,width:0,height:30)] {
            XCTAssertThrowsError(try CaptureRegion(r).validate(in:bounds))
        }
        var r=CaptureRegion(CGRect(x:0,y:0,width:30,height:30)); r.unit="css-pixel"
        XCTAssertThrowsError(try r.validate(in:bounds))
    }
    func testFullDisplayUsesSourceScaleAndRejectsUnavailableGeometry() throws {
        let region = CaptureRegion(bounds)
        for (scale, width, height) in [(1.0, 1000, 800), (1.5, 1500, 1200), (2.0, 2000, 1600)] {
            let source = try resolveSourceGeometry(region: region, bounds: bounds, scale: scale)
            XCTAssertEqual(source.rect, region)
            XCTAssertEqual(source.pixels, PixelSize(width: width, height: height))
        }
        XCTAssertThrowsError(try resolveSourceGeometry(region: region, bounds: bounds, scale: .nan))
        XCTAssertThrowsError(try resolveSourceGeometry(region: region, bounds: CGRect(x: 0, y: 0, width: 900, height: 800), scale: 2))
    }
    func testHalfPointSourceStaysSeparateFromPaddedAndApprovedOutput() throws {
        let region = CaptureRegion(CGRect(x: 10.5, y: 20.5, width: 320.5, height: 239.5))
        let area = CaptureArea(mode: "region", displayRef: "display", snapshotToken: "layout", region: region)
        XCTAssertThrowsError(try resolveSourceGeometry(region: region, bounds: bounds, scale: 1))
        let source = try resolveSourceGeometry(region: region, bounds: bounds, scale: 2)
        XCTAssertEqual(source.rect, region)
        XCTAssertEqual(source.pixels, PixelSize(width: 641, height: 479))
        let padded = try resolveOutputGeometry(area: area, source: source)
        XCTAssertEqual(padded.outputPixels, PixelSize(width: 642, height: 480))
        XCTAssertEqual(padded.paddingRight, 1)
        XCTAssertEqual(padded.paddingBottom, 1)
        let approved = PixelSize(width: 320, height: 240)
        let alternative = try resolveOutputGeometry(area: area, source: source, approvedOutput: approved)
        XCTAssertEqual(alternative.sourcePixels, padded.sourcePixels)
        XCTAssertEqual(alternative.outputPixels, approved)
        XCTAssertLessThan(alternative.scaleFactor, 1)
        XCTAssertThrowsError(try resolveOutputGeometry(area: area, source: source, approvedOutput: PixelSize(width: 644, height: 480)))
    }
}
