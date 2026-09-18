//! [`Hull`] vertex/triangulation helpers. VRF `RubikonPhysics/Shapes/Hull.cs`.

use super::{Hull, PhysError};

impl Hull {
    /// Hull vertex positions: `m_VertexPositions` in the new on-disk format, or `m_Vertices`
    /// directly in the old one (VRF `Hull.cs` `ParseVertices`, ~413-424). `HalfEdge::origin`
    /// indexes this array directly; [`Self::vertex_indices`] (when present) is a separate
    /// per-vertex outgoing-edge list, unrelated to resolving positions.
    pub fn positions(&self) -> &[[f32; 3]] {
        &self.vertex_positions
    }

    /// Triangulates every face as a fan from its first vertex, in VRF order (`Hull.cs`
    /// `GetFaceTriangles`/`FaceTriangleEnumerable`, ~302, ~369-410; `docs/FORMATS.md` 6.3): walking
    /// CCW half-edges yields outward-facing normals. Returned indices are into
    /// [`Self::positions`] (`HalfEdge::origin` indexes positions directly, never
    /// [`Self::vertex_indices`]).
    ///
    /// A face's edge loop is bounded to at most `edges.len()` steps: a well-formed loop always
    /// closes well within that (it visits a subset of the hull's edges), so exceeding it means a
    /// corrupt `m_nNext` cycle that never returns to its start -- reported as
    /// [`PhysError::FaceLoopNotClosed`] instead of looping forever.
    pub fn triangles(&self) -> Result<Vec<[u32; 3]>, PhysError> {
        let n_edges = self.edges.len();
        let n_pos = self.vertex_positions.len();
        let mut out = Vec::new();

        let origin_of = |edge_index: usize| -> Result<u32, PhysError> {
            let o = self.edges[edge_index].origin as usize;
            if o >= n_pos {
                return Err(PhysError::IndexOutOfRange {
                    path: format!("hull.m_Edges[{edge_index}].m_nOrigin"),
                    index: o,
                    len: n_pos,
                });
            }
            Ok(o as u32)
        };

        for (fi, &start) in self.faces.iter().enumerate() {
            let start = start as usize;
            if start >= n_edges {
                return Err(PhysError::IndexOutOfRange {
                    path: format!("hull.m_Faces[{fi}]"),
                    index: start,
                    len: n_edges,
                });
            }
            let mut e = self.edges[start].next as usize;
            let mut steps = 0usize;
            loop {
                if e == start {
                    break;
                }
                if e >= n_edges {
                    return Err(PhysError::IndexOutOfRange {
                        path: format!("hull face {fi} edge walk (m_nNext)"),
                        index: e,
                        len: n_edges,
                    });
                }
                let n = self.edges[e].next as usize;
                if n == start {
                    break;
                }
                if n >= n_edges {
                    return Err(PhysError::IndexOutOfRange {
                        path: format!("hull face {fi} edge walk (m_nNext)"),
                        index: n,
                        len: n_edges,
                    });
                }
                out.push([origin_of(start)?, origin_of(e)?, origin_of(n)?]);
                e = n;
                steps += 1;
                if steps > n_edges {
                    return Err(PhysError::FaceLoopNotClosed {
                        path: format!("hull.m_Faces[{fi}]"),
                        max_steps: n_edges,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Checks every edge/face index is in range, that every face's edge loop closes within
    /// `edges.len()` steps, the [`Self::vertex_indices`] invariant (`edges[vertex_indices[v]]
    /// .origin == v`) when present, and the Euler characteristic `V - E/2 + F == 2`
    /// (`docs/FORMATS.md` 6.3: edges are stored in `(e, twin)` pairs, so `E` in the formula is
    /// `edges.len() / 2`).
    pub fn validate(&self) -> Result<(), PhysError> {
        let v = self.vertex_positions.len();
        let e = self.edges.len();
        let f = self.faces.len();

        for (i, edge) in self.edges.iter().enumerate() {
            let path = format!("hull.m_Edges[{i}]");
            if edge.next as usize >= e {
                return Err(PhysError::IndexOutOfRange {
                    path: format!("{path}.m_nNext"),
                    index: edge.next as usize,
                    len: e,
                });
            }
            if edge.twin as usize >= e {
                return Err(PhysError::IndexOutOfRange {
                    path: format!("{path}.m_nTwin"),
                    index: edge.twin as usize,
                    len: e,
                });
            }
            if edge.origin as usize >= v {
                return Err(PhysError::IndexOutOfRange {
                    path: format!("{path}.m_nOrigin"),
                    index: edge.origin as usize,
                    len: v,
                });
            }
            if edge.face as usize >= f {
                return Err(PhysError::IndexOutOfRange {
                    path: format!("{path}.m_nFace"),
                    index: edge.face as usize,
                    len: f,
                });
            }
        }

        if let Some(indices) = &self.vertex_indices {
            for (vi, &edge_idx) in indices.iter().enumerate() {
                let edge_idx = edge_idx as usize;
                if edge_idx >= e {
                    return Err(PhysError::IndexOutOfRange {
                        path: format!("hull.m_Vertices[{vi}]"),
                        index: edge_idx,
                        len: e,
                    });
                }
                let origin = self.edges[edge_idx].origin as usize;
                if origin != vi {
                    return Err(PhysError::VertexOutgoingEdgeMismatch {
                        path: format!("hull.m_Vertices[{vi}]"),
                        vertex: vi,
                        edge_index: edge_idx,
                        edge_origin: origin,
                    });
                }
            }
        }

        for (i, &start) in self.faces.iter().enumerate() {
            if start as usize >= e {
                return Err(PhysError::IndexOutOfRange {
                    path: format!("hull.m_Faces[{i}]"),
                    index: start as usize,
                    len: e,
                });
            }
        }

        // Every `m_nNext` index above is already known to be < e, so this walk can't index out
        // of bounds; it only needs to be bounded against a cycle that never returns to `start`.
        for (fi, &start) in self.faces.iter().enumerate() {
            let start = start as usize;
            let mut current = self.edges[start].next as usize;
            let mut steps = 0usize;
            while current != start {
                steps += 1;
                if steps > e {
                    return Err(PhysError::FaceLoopNotClosed {
                        path: format!("hull.m_Faces[{fi}]"),
                        max_steps: e,
                    });
                }
                current = self.edges[current].next as usize;
            }
        }

        let euler = v as i64 - (e as i64) / 2 + f as i64;
        if !e.is_multiple_of(2) || euler != 2 {
            return Err(PhysError::EulerCheck {
                path: "hull".to_string(),
                v,
                e,
                f,
                result: euler,
            });
        }
        Ok(())
    }
}
